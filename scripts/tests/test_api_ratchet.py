"""Unit tests for scripts/api_ratchet.py."""
from __future__ import annotations

import json
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import api_ratchet as ar  # noqa: E402


def _minimal_rustdoc(*extra_index: dict) -> dict:
    index: dict[str, dict] = {
        "0": {
            "id": 0,
            "crate_id": 0,
            "name": "grit_lib",
            "visibility": "public",
            "inner": {"module": {"is_stripped": False, "items": list(range(1, 1 + len(extra_index)))}},
        },
    }
    for idx, item in enumerate(extra_index, start=1):
        entry = dict(item)
        entry.setdefault("id", idx)
        entry.setdefault("crate_id", 0)
        entry.setdefault("visibility", "public")
        index[str(idx)] = entry
    paths = {
        "0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
    }
    for idx, item in enumerate(extra_index, start=1):
        name = item.get("name", f"item{idx}")
        paths[str(idx)] = {
            "crate_id": 0,
            "path": ["grit_lib", name],
            "kind": "function",
        }
    return {"index": index, "paths": paths}


class ApiRatchetMetricTests(unittest.TestCase):
    def test_public_api_lines(self) -> None:
        snap = ar.compute_metrics(
            public_api_text="line one\n\nline two\n",
            payload={"index": {}, "paths": {}},
        )
        self.assertEqual(snap.metrics["public_api_lines"].count, 2)

    def test_with_in_name_and_two_bools_and_result_string(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "open_with_options",
                "inner": {
                    "function": {
                        "sig": {
                            "inputs": [
                                ["a", {"type": {"primitive": "bool"}}],
                                ["b", {"type": {"primitive": "bool"}}],
                            ],
                            "output": {
                                "resolved_path": {
                                    "path": "core::result::Result",
                                    "args": {
                                        "angle_bracketed": {
                                            "args": [
                                                {"type": {"primitive": "unit"}},
                                                {"type": {"resolved_path": {"path": "String"}}},
                                            ]
                                        }
                                    },
                                }
                            },
                        },
                        "header": {"is_unsafe": False},
                    }
                },
            }
        )
        snap = ar.compute_metrics(public_api_text="x", payload=payload)
        self.assertEqual(snap.metrics["pub_fn_with_in_name"].count, 1)
        self.assertEqual(snap.metrics["pub_fn_two_plus_bare_bool"].count, 1)
        self.assertEqual(snap.metrics["pub_fn_result_string_err"].count, 1)

    def test_repository_first_and_git_dir_first(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "run",
                "inner": {
                    "function": {
                        "sig": {
                            "inputs": [
                                [
                                    "repo",
                                    {
                                        "borrowed_ref": {
                                            "is_mutable": False,
                                            "type": {
                                                "resolved_path": {"path": "Repository"}
                                            },
                                        }
                                    },
                                ]
                            ],
                            "output": None,
                        },
                        "header": {},
                    }
                },
            },
            {
                "name": "open",
                "inner": {
                    "function": {
                        "sig": {
                            "inputs": [
                                [
                                    "git_dir",
                                    {
                                        "borrowed_ref": {
                                            "is_mutable": False,
                                            "type": {
                                                "resolved_path": {"path": "std::path::Path"}
                                            },
                                        }
                                    },
                                ]
                            ],
                            "output": None,
                        },
                        "header": {},
                    }
                },
            },
        )
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_free_fn_repository_first"].count, 1)
        self.assertEqual(snap.metrics["pub_free_fn_git_dir_path_first"].count, 1)

    def test_unsafe_and_libc(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "danger",
                "inner": {
                    "function": {
                        "sig": {
                            "inputs": [
                                [
                                    "t",
                                    {"resolved_path": {"path": "libc::time_t"}},
                                ]
                            ],
                            "output": None,
                        },
                        "header": {"is_unsafe": True},
                    }
                },
            }
        )
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_unsafe_fn"].count, 1)
        self.assertEqual(snap.metrics["pub_libc_types"].count, 1)

    def test_too_many_arguments_from_attrs(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "wide",
                "attrs": [{"other": "#[allow(clippy::too_many_arguments)]"}],
                "inner": {
                    "function": {
                        "sig": {"inputs": [], "output": None},
                        "header": {},
                    }
                },
            }
        )
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_fn_too_many_arguments_allow"].count, 1)

    def test_too_many_arguments_from_source_span(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src_root = Path(tmp) / "grit-lib" / "src"
            src_root.mkdir(parents=True)
            rs = src_root / "demo.rs"
            rs.write_text(
                textwrap.dedent(
                    """\
                    #[allow(clippy::too_many_arguments)]
                    pub fn from_source() {}
                    """
                ),
                encoding="utf-8",
            )
            payload = _minimal_rustdoc(
                {
                    "name": "from_source",
                    "attrs": [],
                    "span": {
                        "filename": str(rs),
                        "begin": [3, 1],
                        "end": [3, 20],
                    },
                    "inner": {
                        "function": {
                            "sig": {"inputs": [], "output": None},
                            "header": {},
                        }
                    },
                }
            )
            old_grit = ar.GRIT_LIB
            ar.GRIT_LIB = Path(tmp) / "grit-lib"
            try:
                snap = ar.compute_metrics(public_api_text="", payload=payload)
            finally:
                ar.GRIT_LIB = old_grit
            self.assertEqual(
                snap.metrics["pub_fn_too_many_arguments_allow"].count,
                1,
            )

    def test_multiple_paths_and_prelude_exemption(self) -> None:
        payload = {
            "index": {
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [2, 3, 4]}},
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "name": "prelude",
                    "visibility": "public",
                    "inner": {
                        "module": {
                            "items": [
                                5,
                            ]
                        }
                    },
                },
                "3": {
                    "id": 3,
                    "crate_id": 0,
                    "name": "root_mod",
                    "visibility": "public",
                    "inner": {
                        "module": {
                            "items": [
                                6,
                            ]
                        }
                    },
                },
                "4": {
                    "id": 4,
                    "crate_id": 0,
                    "visibility": "public",
                    "inner": {
                        "use": {
                            "name": "Thing",
                            "id": 7,
                            "is_glob": False,
                            "source": "crate::thing::Thing",
                        }
                    },
                },
                "5": {
                    "id": 5,
                    "crate_id": 0,
                    "visibility": "public",
                    "inner": {
                        "use": {
                            "name": "Thing",
                            "id": 7,
                            "is_glob": False,
                            "source": "crate::thing::Thing",
                        }
                    },
                },
                "6": {
                    "id": 6,
                    "crate_id": 0,
                    "visibility": "public",
                    "inner": {
                        "use": {
                            "name": "Thing",
                            "id": 7,
                            "is_glob": False,
                            "source": "crate::thing::Thing",
                        }
                    },
                },
                "7": {
                    "id": 7,
                    "crate_id": 0,
                    "name": "Thing",
                    "visibility": "public",
                    "inner": {"struct": {"kind": "plain"}},
                },
            },
            "paths": {
                "1": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
            },
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_items_multiple_paths"].count, 1)

        payload_prelude_only = {
            "index": {
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [2]}},
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "name": "prelude",
                    "visibility": "public",
                    "inner": {
                        "module": {
                            "items": [
                                3,
                                4,
                            ]
                        }
                    },
                },
                "3": {
                    "id": 3,
                    "crate_id": 0,
                    "visibility": "public",
                    "inner": {
                        "use": {
                            "name": "X",
                            "id": 5,
                            "is_glob": False,
                        }
                    },
                },
                "4": {
                    "id": 4,
                    "crate_id": 0,
                    "visibility": "public",
                    "inner": {
                        "use": {
                            "name": "Y",
                            "id": 5,
                            "is_glob": False,
                        }
                    },
                },
                "5": {
                    "id": 5,
                    "crate_id": 0,
                    "name": "Shared",
                    "visibility": "public",
                    "inner": {"struct": {"kind": "plain"}},
                },
            },
            "paths": {
                "1": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
            },
        }
        snap2 = ar.compute_metrics(public_api_text="", payload=payload_prelude_only)
        self.assertEqual(snap2.metrics["pub_items_multiple_paths"].count, 0)

    def test_compare_fails_on_increase(self) -> None:
        snap = ar.RatchetSnapshot(
            metrics={
                key: ar.MetricResult(2 if key == "pub_unsafe_fn" else 0)
                for key in ar.METRIC_KEYS
            }
        )
        baseline = {key: 0 for key in ar.METRIC_KEYS}
        ok, failures = ar.compare(baseline, snap)
        self.assertFalse(ok)
        self.assertTrue(any("pub_unsafe_fn" in line for line in failures))

    def test_compare_fails_on_unrecorded_decrease(self) -> None:
        snap = ar.RatchetSnapshot(
            metrics={key: ar.MetricResult(0) for key in ar.METRIC_KEYS}
        )
        baseline = {key: 1 for key in ar.METRIC_KEYS}
        ok, failures = ar.compare(baseline, snap)
        self.assertFalse(ok)
        self.assertTrue(
            any("make api-ratchet" in line for line in failures),
        )


if __name__ == "__main__":
    unittest.main()
