use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Task {
    Roof,
    Sky,
    Quality,
}

impl Task {
    pub fn labels(self) -> &'static [&'static str] {
        match self {
            Self::Roof => &["open", "partial", "closed"],
            Self::Sky => &["clear", "partly_cloudy", "overcast"],
            Self::Quality => &["usable", "too_dark", "saturated", "obscured"],
        }
    }
}

/// Contract v1: RGB f32 NCHW, letterboxed ROI, one [1, classes] logits output.
/// Separate task models allow site-specific roof and sky crops. Training must
/// use exactly this preprocessing and Task::labels() order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    pub schema_version: u32,
    pub model_id: String,
    pub sha256: String,
    pub task: Task,
    pub width: u32,
    pub height: u32,
    pub mean: [f32; 3],
    pub std: [f32; 3],
    pub min_confidence: f32,
}

impl ModelSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "Unsupported model schema");
        ensure!(
            !self.model_id.trim().is_empty() && self.model_id.len() <= 128,
            "Invalid model ID"
        );
        ensure!(
            self.sha256.len() == 64 && self.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid model SHA256"
        );
        ensure!(
            (16..=512).contains(&self.width) && (16..=512).contains(&self.height),
            "Input dimensions must be 16–512"
        );
        ensure!(
            self.mean.iter().all(|x| x.is_finite())
                && self.std.iter().all(|x| x.is_finite() && *x > 0.0),
            "Invalid normalization"
        );
        ensure!(
            self.min_confidence.is_finite() && (0.5..=1.0).contains(&self.min_confidence),
            "Confidence threshold must be 0.5–1"
        );
        Ok(())
    }

    pub fn classify(&self, logits: &[f32]) -> Result<Prediction> {
        self.validate()?;
        let labels = self.task.labels();
        ensure!(
            logits.len() == labels.len() && logits.iter().all(|v| v.is_finite()),
            "Invalid model logits"
        );
        let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let weights: Vec<_> = logits.iter().map(|x| (x - max).exp()).collect();
        let sum: f32 = weights.iter().sum();
        let probabilities: Vec<_> = weights.iter().map(|x| x / sum).collect();
        let best = probabilities
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        Ok(Prediction {
            model_id: self.model_id.clone(),
            task: self.task,
            label: (probabilities[best] >= self.min_confidence).then(|| labels[best].to_string()),
            confidence: probabilities[best],
            probabilities,
        })
    }
}

/// `label: None` means uncertain. Scores are not calibrated probabilities until
/// calibrated and evaluated on held-out observing sessions.
#[derive(Debug, Serialize)]
pub struct Prediction {
    pub model_id: String,
    pub task: Task,
    pub label: Option<String>,
    pub confidence: f32,
    pub probabilities: Vec<f32>,
}

#[cfg(test)]
pub(crate) fn test_spec() -> ModelSpec {
    ModelSpec {
        schema_version: 1,
        model_id: "synthetic-test-only".into(),
        sha256: "0".repeat(64),
        task: Task::Roof,
        width: 16,
        height: 16,
        mean: [0.0; 3],
        std: [1.0; 3],
        min_confidence: 0.8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uncertain_finite_and_stable_softmax() {
        let spec = test_spec();
        assert!(spec.classify(&[0.0; 3]).unwrap().label.is_none());
        assert_eq!(
            spec.classify(&[10000.0, 0.0, -10000.0])
                .unwrap()
                .label
                .as_deref(),
            Some("open")
        );
        assert!(spec.classify(&[f32::NAN, 0.0, 1.0]).is_err());
        assert!(spec.classify(&[1.0]).is_err());
    }
    #[test]
    fn rejects_invalid_contract() {
        let mut spec = test_spec();
        spec.std[0] = 0.0;
        assert!(spec.validate().is_err());
        spec = test_spec();
        spec.schema_version = 2;
        assert!(spec.validate().is_err());
        spec = test_spec();
        spec.width = 100_000;
        assert!(spec.validate().is_err());
    }
}
