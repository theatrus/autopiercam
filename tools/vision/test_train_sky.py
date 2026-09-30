import copy
import unittest

from train_sky import LABELS, metrics, select_samples


class TrainingContractTests(unittest.TestCase):
    def fixture(self):
        manifest = {"schema_version": 1, "samples": {}}
        split = {"schema_version": 1, "dataset_sha256": "snapshot",
                 "label_source": "synthetic", "session_method": "explicit", "samples": {}}
        for partition in ("train", "test"):
            for label in LABELS:
                key = partition + label
                manifest["samples"][key] = {"image": f"images/{key}.jpg", "site": "site", "camera": "camera",
                                            "labels": {"sky": label, "roof": "open", "quality": "usable"}}
                split["samples"][key] = {"session": partition, "split": partition}
        return manifest, split

    def test_whole_session_split(self):
        manifest, split = self.fixture()
        self.assertEqual(len(select_samples(manifest, split, "snapshot")), 6)
        split["samples"]["testclear"]["session"] = "train"
        with self.assertRaisesRegex(ValueError, "leaks"):
            select_samples(manifest, split, "snapshot")

    def test_changed_snapshot_and_missing_assignments_rejected(self):
        manifest, split = self.fixture()
        with self.assertRaisesRegex(ValueError, "snapshot"):
            select_samples(manifest, split, "different")
        del split["samples"]["trainclear"]
        with self.assertRaisesRegex(ValueError, "Missing"):
            select_samples(manifest, split, "snapshot")

    def test_uncertain_hidden_and_bad_quality_excluded(self):
        manifest, split = self.fixture()
        for field, value in (("sky", "uncertain"), ("sky", "not_visible"),
                             ("quality", "too_dark"), ("roof", "closed")):
            extra = copy.deepcopy(manifest["samples"]["trainclear"])
            extra["labels"][field] = value
            manifest["samples"]["excluded"] = extra
            self.assertEqual(len(select_samples(manifest, split, "snapshot")), 6)

    def test_missing_class_and_path_escape_rejected(self):
        manifest, split = self.fixture()
        manifest["samples"]["testclear"]["labels"]["sky"] = "uncertain"
        with self.assertRaisesRegex(ValueError, "every sky class"):
            select_samples(manifest, split, "snapshot")
        manifest, split = self.fixture()
        manifest["samples"]["trainclear"]["image"] = "../outside.jpg"
        with self.assertRaisesRegex(ValueError, "image path"):
            select_samples(manifest, split, "snapshot")

    def test_metrics_and_abstention(self):
        result = metrics([0, 1, 2], [0, 1, 1], [0.9, 0.6, 0.9])
        self.assertEqual(result["accuracy"], 2 / 3)
        self.assertEqual(result["accepted"], 2)
        self.assertEqual(result["accepted_accuracy"], 0.5)
        self.assertIsNone(metrics([0], [1], [0.4])["accepted_accuracy"])


if __name__ == "__main__":
    unittest.main()
