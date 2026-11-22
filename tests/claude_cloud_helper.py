"""Offline safety tests for the opt-in Claude teleport helper."""
import json
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest
import uuid

helper = runpy.run_path(str(Path(__file__).resolve().parents[1] / "tools/claude-cloud-export"))


class HelperSafety(unittest.TestCase):
    def test_cleanup_only_unlinks_our_matching_transcript(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            project = base / "project"
            project.mkdir()
            actual = str(base / "worktree")
            sid = str(uuid.uuid4())
            path = project / (sid + ".jsonl")
            other = project / "must-not-remove.jsonl"
            other.write_text("other transcript")
            path.write_text(json.dumps({"cwd": actual + "-different", "sessionId": sid}))
            with self.assertRaises(ValueError):
                helper["remove_temporary_transcript"](base, sid, Path(actual))
            self.assertTrue(path.exists())
            path.write_text(json.dumps({"cwd": actual, "sessionId": sid}))
            helper["remove_temporary_transcript"](base, sid, Path(actual))
            self.assertFalse(path.exists())
            self.assertEqual(other.read_text(), "other transcript")

    def test_checkout_config_disables_repo_filters(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp) / "repo"
            subprocess.run(["git", "init", "--quiet", str(repo)], check=True)
            subprocess.run(["git", "-C", str(repo), "config", "filter.probe.smudge", "false"],
                           check=True)
            subprocess.run(["git", "-C", str(repo), "config", "filter.probe.process", "false"],
                           check=True)
            env = helper["git_env"](repo)
            for key in ("filter.probe.smudge", "filter.probe.process"):
                value = subprocess.check_output(
                    ["git", "-C", str(repo), "config", "--get", key], env=env,
                ).strip()
                self.assertEqual(value, b"")
            self.assertEqual(env.get("GIT_ALLOW_PROTOCOL"), "")


if __name__ == "__main__":
    unittest.main()
