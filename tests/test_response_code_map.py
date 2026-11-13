"""Qoder's opt-in response classifications through the shared Core probe."""
import json
import os
from pathlib import Path
import subprocess
import unittest


REPO = Path(__file__).resolve().parents[1]
MAPPING = REPO / "tests/fixtures/response-code-map.json"


@unittest.skipUnless(os.environ.get("CORE_RUNNER"), "set CORE_RUNNER to the built Core test runner")
class ResponseCodeMappingTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mapping = json.loads(MAPPING.read_text(encoding="utf-8"))

    def classify(self, body):
        read_fd, write_fd = os.pipe()
        try:
            env = os.environ.copy()
            env["STRAVIA_ALIGNMENT_REPORT_FD"] = str(write_fd)
            subprocess.run(
                [env["CORE_RUNNER"], "--ignored", "--exact",
                 "plugin::execution::alignment_tests::response_code_map_probe"],
                input=json.dumps({"response_code_map": self.mapping, "body": body}).encode(),
                env=env, pass_fds=(write_fd,), check=True, capture_output=True,
            )
            os.close(write_fd)
            write_fd = -1
            with os.fdopen(read_fd, "rb") as report:
                read_fd = -1
                return json.load(report)["category"]
        finally:
            if write_fd != -1:
                os.close(write_fd)
            if read_fd != -1:
                os.close(read_fd)

    def test_all_original_qoder_codes(self):
        expected = {
            105: "login_expired", 103: "duplicate_request",
            110: "quota_or_billing", 113: "quota_or_billing", 114: "quota_or_billing",
            115: "quota_or_billing", 116: "quota_or_billing", 117: "quota_or_billing",
            118: "quota_or_billing", 119: "quota_or_billing", 122: "quota_or_billing",
            406: "content_policy", 430: "unsupported", 100400: "custom_model_service",
            100401: "custom_model_auth", 100403: "custom_model_unavailable",
            10605: "queued", 80411: "input_limits", 80412: "input_limits",
        }
        for code, category in expected.items():
            with self.subTest(code=code):
                self.assertEqual(self.classify({"code": code}), category)

    def test_each_field_accepts_integer_and_numeric_string(self):
        for field in ("code", "type", "status", "error"):
            for code in (105, "105"):
                with self.subTest(field=field, code=code):
                    self.assertEqual(self.classify({field: code}), "login_expired")

    def test_nested_structured_and_encoded_json(self):
        self.assertEqual(self.classify({"data": [{"error": {"status": 406}}]}), "content_policy")
        self.assertEqual(self.classify({"data": '{"code":105}'}), "login_expired")

    def test_unknown_codes_and_unmapped_fields_stay_unknown(self):
        for body in ({"code": 6004}, {"code": 999}, {"message": 105}, {"data": "opaque 105"}):
            with self.subTest(body=body):
                self.assertEqual(self.classify(body), "unknown")


if __name__ == "__main__":
    unittest.main()
