//! CPU-only Rust ONNX execution. Load only trusted models whose digest was
//! obtained from a trusted manifest. A checksum is not a model sandbox.
use crate::{
    model::{ModelSpec, Prediction},
    preprocess::{self, Roi},
    read_bounded, sha256,
};
use anyhow::{Result, ensure};
use std::{io::Cursor, path::Path, sync::Arc};
use tract_onnx::prelude::*;
use tract_onnx::tract_hir::infer::Factoid;

pub struct Classifier {
    spec: ModelSpec,
    plan: Arc<TypedRunnableModel>,
}

impl Classifier {
    pub fn load(model_path: &Path, spec: ModelSpec) -> Result<Self> {
        spec.validate()?;
        let bytes = read_bounded(model_path, 64 * 1024 * 1024)?;
        ensure!(
            sha256(&bytes).eq_ignore_ascii_case(&spec.sha256),
            "Model checksum mismatch"
        );
        let model = tract_onnx::onnx().model_for_read(&mut Cursor::new(bytes))?;
        ensure!(
            model.input_outlets()?.len() == 1 && model.output_outlets()?.len() == 1,
            "Model must have exactly one input and output"
        );
        let expected = f32::fact([1, 3, spec.height as usize, spec.width as usize]).into();
        let input = model.input_fact(0)?.unify(&expected)?;
        let model = model.with_input_fact(0, input)?.into_optimized()?;
        let output = model.output_fact(0)?;
        ensure!(
            output.datum_type == f32::datum_type()
                && output.shape.as_concrete() == Some(&[1, spec.task.labels().len()]),
            "Output must be f32 [1, classes] logits in the documented label order"
        );
        Ok(Self {
            spec,
            plan: model.into_runnable()?,
        })
    }

    pub fn predict(&self, image: &[u8], roi: Roi) -> Result<Prediction> {
        let data = preprocess::prepare(image, &self.spec, roi)?;
        let input = Tensor::from_shape(
            &[1, 3, self.spec.height as usize, self.spec.width as usize],
            &data,
        )?;
        let output = self.plan.run(tvec!(input.into()))?;
        self.spec
            .classify(output[0].try_as_plain_ram()?.as_slice::<f32>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use tract_onnx::pb::*;

    fn value(name: &str, shape: &[i64]) -> ValueInfoProto {
        ValueInfoProto {
            name: name.into(),
            r#type: Some(TypeProto {
                value: Some(type_proto::Value::TensorType(type_proto::Tensor {
                    elem_type: 1,
                    shape: Some(TensorShapeProto {
                        dim: shape
                            .iter()
                            .map(|d| tensor_shape_proto::Dimension {
                                value: Some(tensor_shape_proto::dimension::Value::DimValue(*d)),
                                ..Default::default()
                            })
                            .collect(),
                    }),
                })),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    // Hand-authored color classifier, NOT a trained roof/sky model. Exercises
    // ONNX parsing, convolution, activation, pooling, layout and CPU execution.
    fn smoke_model() -> Vec<u8> {
        ModelProto {
            ir_version: 8,
            opset_import: vec![OperatorSetIdProto {
                domain: String::new(),
                version: 17,
            }],
            graph: Some(GraphProto {
                name: "synthetic-test-only".into(),
                input: vec![value("input", &[1, 3, 16, 16])],
                output: vec![value("logits", &[1, 3])],
                initializer: vec![TensorProto {
                    name: "weights".into(),
                    dims: vec![3, 3, 1, 1],
                    data_type: 1,
                    float_data: vec![8.0, 0.0, 0.0, 0.0, 8.0, 0.0, 0.0, 0.0, 8.0],
                    ..Default::default()
                }],
                node: vec![
                    NodeProto {
                        op_type: "Conv".into(),
                        input: vec!["input".into(), "weights".into()],
                        output: vec!["conv".into()],
                        ..Default::default()
                    },
                    NodeProto {
                        op_type: "Relu".into(),
                        input: vec!["conv".into()],
                        output: vec!["relu".into()],
                        ..Default::default()
                    },
                    NodeProto {
                        op_type: "GlobalAveragePool".into(),
                        input: vec!["relu".into()],
                        output: vec!["pool".into()],
                        ..Default::default()
                    },
                    NodeProto {
                        op_type: "Flatten".into(),
                        input: vec!["pool".into()],
                        output: vec!["logits".into()],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn real_onnx_cpu_inference_matches_known_logits_without_python_or_camera() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.onnx");
        let bytes = smoke_model();
        std::fs::write(&path, &bytes).unwrap();
        let mut spec = crate::model::test_spec();
        spec.sha256 = sha256(&bytes);
        let model = Classifier::load(&path, spec).unwrap();
        for (color, label) in [
            ([255, 0, 0], "open"),
            ([0, 255, 0], "partial"),
            ([0, 0, 255], "closed"),
        ] {
            let prediction = model
                .predict(&crate::preprocess::png(16, 16, color), Roi::default())
                .unwrap();
            assert_eq!(prediction.label.as_deref(), Some(label));
            assert!((prediction.confidence - 1.0 / (1.0 + 2.0 * (-8.0_f32).exp())).abs() < 1e-5);
        }
        assert!(
            model
                .predict(&crate::preprocess::png(16, 16, [80; 3]), Roi::default())
                .unwrap()
                .label
                .is_none()
        );
    }

    #[test]
    fn mismatched_checksum_and_output_contract_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.onnx");
        let bytes = smoke_model();
        std::fs::write(&path, &bytes).unwrap();
        let mut spec = crate::model::test_spec();
        assert!(Classifier::load(&path, spec.clone()).is_err());
        spec.sha256 = sha256(&bytes);
        let mut wrong_input = spec.clone();
        wrong_input.width = 32;
        assert!(Classifier::load(&path, wrong_input).is_err());
        spec.task = crate::model::Task::Quality;
        assert!(Classifier::load(&path, spec).is_err());
    }
}
