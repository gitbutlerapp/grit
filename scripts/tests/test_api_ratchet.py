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


class ErrorRatchetMetricTests(unittest.TestCase):
    def test_error_message_construction_sites(self) -> None:
        count = ar.count_error_message_construction_sites()
        self.assertGreaterEqual(count, 300)

    def test_collect_error_ratchet_keys(self) -> None:
        snap = ar.compute_metrics(public_api_text="", payload={"index": {}, "paths": {}})
        for key in (
            "umbrella_error_variants",
            "pub_error_catch_all_variants",
            "pub_error_string_tuple_variants",
            "pub_error_enum_not_non_exhaustive",
            "pub_area_error_wraps_umbrella",
            "error_message_construction_sites",
        ):
            self.assertIn(key, snap.metrics)


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

    def test_result_string_through_type_alias(self) -> None:
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [1, 2, 3]}},
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "StrResult",
                    "visibility": "public",
                    "inner": {
                        "type_alias": {
                            "type": {
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
                            }
                        }
                    },
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "name": "OuterAlias",
                    "visibility": "public",
                    "inner": {
                        "type_alias": {
                            "type": {"resolved_path": {"path": "StrResult", "id": 1}}
                        }
                    },
                },
                "3": {
                    "id": 3,
                    "crate_id": 0,
                    "name": "via_alias",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {
                                "inputs": [],
                                "output": {"resolved_path": {"path": "OuterAlias", "id": 2}},
                            },
                            "header": {},
                        }
                    },
                },
            },
            "paths": {"0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"}},
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_fn_result_string_err"].count, 1)

    def test_free_fn_repository_excludes_assoc_and_near_types(self) -> None:
        repo_ref = {
            "borrowed_ref": {
                "is_mutable": False,
                "type": {"resolved_path": {"path": "Repository"}},
            }
        }
        opts_ref = {
            "borrowed_ref": {
                "is_mutable": False,
                "type": {"resolved_path": {"path": "RepositoryOptions"}},
            }
        }
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [10, 12, 13]}},
                },
                "10": {
                    "id": 10,
                    "crate_id": 0,
                    "visibility": "public",
                    "inner": {"impl": {"items": [11], "for": {"resolved_path": {"path": "Repository"}}}},
                },
                "11": {
                    "id": 11,
                    "crate_id": 0,
                    "name": "assoc_static",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {"inputs": [["repo", repo_ref]], "output": None},
                            "header": {},
                        }
                    },
                },
                "12": {
                    "id": 12,
                    "crate_id": 0,
                    "name": "free_fn",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {"inputs": [["repo", repo_ref]], "output": None},
                            "header": {},
                        }
                    },
                },
                "13": {
                    "id": 13,
                    "crate_id": 0,
                    "name": "opts_fn",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {"inputs": [["repo", opts_ref]], "output": None},
                            "header": {},
                        }
                    },
                },
            },
            "paths": {"0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"}},
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_free_fn_repository_first"].count, 1)

    def test_glob_reexport_multiple_paths(self) -> None:
        payload = {
            "index": {
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [2, 4]}},
                },
                "2": {
                    "id": 2,
                    "name": "inner",
                    "visibility": "public",
                    "inner": {"module": {"items": [3]}},
                },
                "3": {
                    "id": 3,
                    "name": "Exported",
                    "visibility": "public",
                    "inner": {"struct": {"kind": "plain"}},
                },
                "4": {
                    "id": 4,
                    "visibility": "public",
                    "inner": {
                        "use": {
                            "name": "inner",
                            "id": 2,
                            "is_glob": True,
                            "source": "crate::inner",
                        }
                    },
                },
                "5": {
                    "id": 5,
                    "name": "mirror",
                    "visibility": "public",
                    "inner": {"module": {"items": [6]}},
                },
                "6": {
                    "id": 6,
                    "visibility": "public",
                    "inner": {
                        "use": {
                            "name": "Exported",
                            "id": 3,
                            "is_glob": False,
                        }
                    },
                },
            },
            "paths": {"1": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"}},
        }
        payload["index"]["1"]["inner"]["module"]["items"].append(5)
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertGreaterEqual(snap.metrics["pub_items_multiple_paths"].count, 1)

        payload_glob_only = {
            "index": {
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [2, 4]}},
                },
                "2": {
                    "id": 2,
                    "name": "inner",
                    "visibility": "public",
                    "inner": {"module": {"items": [3]}},
                },
                "3": {
                    "id": 3,
                    "name": "Only",
                    "visibility": "public",
                    "inner": {"struct": {"kind": "plain"}},
                },
                "4": {
                    "id": 4,
                    "visibility": "public",
                    "inner": {"use": {"name": "inner", "id": 2, "is_glob": True}},
                },
            },
            "paths": {"1": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"}},
        }
        paths = ar.collect_public_paths(payload_glob_only)
        self.assertIn("grit_lib::Only", paths.get("3", []))

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

    def test_pub_cancel_types_dyn_cancel_param(self) -> None:
        cancel_trait = {
            "name": "Cancel",
            "inner": {"trait": {"items": [], "bounds": []}},
        }
        payload = _minimal_rustdoc(
            cancel_trait,
            {
                "name": "run",
                "inner": {
                    "function": {
                        "sig": {
                            "inputs": [
                                [
                                    "cancel",
                                    {
                                        "borrowed_ref": {
                                            "is_mutable": False,
                                            "type": {
                                                "dyn_trait": {
                                                    "traits": [
                                                        {
                                                            "trait": {
                                                                "path": "Cancel",
                                                                "id": 99,
                                                                "args": None,
                                                            }
                                                        }
                                                    ]
                                                }
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
        self.assertGreaterEqual(snap.metrics["pub_cancel_types"].count, 2)

    def test_pub_progress_interfaces_trait_and_fnmut(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "Progress",
                "inner": {
                    "trait": {
                        "items": [],
                        "bounds": [
                            {
                                "trait_bound": {
                                    "trait": {"path": "ProgressSink", "id": 1},
                                    "generic_params": [],
                                    "modifier": "none",
                                }
                            }
                        ],
                    }
                },
            },
            {
                "name": "cb",
                "inner": {
                    "function": {
                        "sig": {
                            "inputs": [
                                [
                                    "f",
                                    {
                                        "borrowed_ref": {
                                            "is_mutable": True,
                                            "type": {
                                                "dyn_trait": {
                                                    "traits": [
                                                        {
                                                            "trait": {
                                                                "path": "FnMut",
                                                                "id": 2,
                                                                "args": {
                                                                    "parenthesized": {
                                                                        "inputs": [
                                                                            {
                                                                                "resolved_path": {
                                                                                    "path": "PackProgress",
                                                                                    "id": 3,
                                                                                }
                                                                            }
                                                                        ],
                                                                        "output": None,
                                                                    }
                                                                },
                                                            }
                                                        }
                                                    ]
                                                }
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
        self.assertEqual(snap.metrics["pub_progress_interfaces"].count, 2)

    def test_pub_refname_validators(self) -> None:
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [1, 2]}},
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "refname_is_safe",
                    "visibility": "public",
                    "span": {"filename": "grit-lib/src/check_ref_format.rs", "begin": [1, 1]},
                    "inner": {
                        "function": {
                            "sig": {
                                "inputs": [["name", {"type": {"primitive": "str"}}]],
                                "output": {"primitive": "bool"},
                            },
                            "header": {},
                        }
                    },
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "name": "other",
                    "visibility": "public",
                    "span": {"filename": "grit-lib/src/lib.rs", "begin": [1, 1]},
                    "inner": {
                        "function": {
                            "sig": {
                                "inputs": [],
                                "output": {
                                    "resolved_path": {
                                        "path": "Result",
                                        "args": {
                                            "angle_bracketed": {
                                                "args": [
                                                    {"type": {"tuple": []}},
                                                    {"type": {"primitive": "u8"}},
                                                ]
                                            }
                                        },
                                    }
                                },
                            },
                            "header": {},
                        }
                    },
                },
            },
            "paths": {"0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"}},
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_refname_validators"].count, 1)

    def test_pub_fn_refname_str(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "resolve_ref",
                "span": {"filename": "grit-lib/src/refs/mod.rs", "begin": [1, 1]},
                "inner": {
                    "function": {
                        "sig": {
                            "inputs": [
                                [
                                    "refname",
                                    {
                                        "borrowed_ref": {
                                            "is_mutable": False,
                                            "type": {"primitive": "str"},
                                        }
                                    },
                                ]
                            ],
                            "output": None,
                        },
                        "header": {},
                    }
                },
            }
        )
        payload["paths"]["1"] = {
            "crate_id": 0,
            "path": ["grit_lib", "refs", "resolve_ref"],
            "kind": "function",
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_fn_refname_str"].count, 1)

    def test_pub_fn_refname_str_public_trait_default_method(self) -> None:
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [1, 2]}},
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "RefStore",
                    "visibility": "public",
                    "inner": {"trait": {"items": [2], "bounds": []}},
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "name": "read_raw",
                    "visibility": "default",
                    "inner": {
                        "function": {
                            "sig": {
                                "inputs": [
                                    [
                                        "self",
                                        {
                                            "borrowed_ref": {
                                                "is_mutable": False,
                                                "type": {"generic": "Self"},
                                            }
                                        },
                                    ],
                                    [
                                        "name",
                                        {
                                            "borrowed_ref": {
                                                "is_mutable": False,
                                                "type": {"primitive": "str"},
                                            }
                                        },
                                    ],
                                ],
                                "output": None,
                            },
                            "header": {},
                        }
                    },
                },
            },
            "paths": {
                "0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
                "1": {
                    "crate_id": 0,
                    "path": ["grit_lib", "refs", "store", "RefStore"],
                    "kind": "trait",
                },
            },
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_fn_refname_str"].count, 1)
        self.assertIn("RefStore::read_raw", snap.metrics["pub_fn_refname_str"].offenders[0])

    def test_pub_fn_pathspec_strings_resolved_pathspecs_fields(self) -> None:
        vec_string_field = {
            "struct_field": {
                "resolved_path": {
                    "path": "Vec",
                    "id": 85,
                    "args": {
                        "angle_bracketed": {
                            "args": [
                                {
                                    "type": {
                                        "resolved_path": {
                                            "path": "String",
                                            "id": 86,
                                            "args": None,
                                        }
                                    }
                                }
                            ],
                            "constraints": [],
                        }
                    },
                }
            }
        }
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [1, 2, 3]}},
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "ResolvedPathspecs",
                    "visibility": "public",
                    "inner": {
                        "struct": {
                            "kind": {"plain": {"fields": [2, 3], "has_stripped_fields": False}},
                            "generics": {"params": [], "where_predicates": []},
                        }
                    },
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "name": "specs",
                    "visibility": "public",
                    "inner": vec_string_field,
                },
                "3": {
                    "id": 3,
                    "crate_id": 0,
                    "name": "sources",
                    "visibility": "public",
                    "inner": vec_string_field,
                },
            },
            "paths": {
                "0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
                "1": {
                    "crate_id": 0,
                    "path": ["grit_lib", "worktree", "pathspec", "ResolvedPathspecs"],
                    "kind": "struct",
                },
            },
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_fn_pathspec_strings"].count, 2)

    def test_pub_cancel_types_tuple_struct_field(self) -> None:
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [1, 2]}},
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "FlagCancel",
                    "visibility": "public",
                    "inner": {
                        "struct": {
                            "kind": {"tuple": [2]},
                            "generics": {"params": [], "where_predicates": []},
                        }
                    },
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "name": "0",
                    "visibility": "public",
                    "inner": {
                        "struct_field": {
                            "resolved_path": {
                                "path": "std::sync::Arc",
                                "id": 1,
                                "args": {
                                    "angle_bracketed": {
                                        "args": [
                                            {
                                                "type": {
                                                    "resolved_path": {
                                                        "path": "std::sync::atomic::AtomicBool",
                                                        "id": 2,
                                                        "args": None,
                                                    }
                                                }
                                            }
                                        ],
                                        "constraints": [],
                                    }
                                },
                            }
                        }
                    },
                },
            },
            "paths": {
                "0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
                "1": {
                    "crate_id": 0,
                    "path": ["grit_lib", "progress", "FlagCancel"],
                    "kind": "struct",
                },
            },
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertGreaterEqual(snap.metrics["pub_cancel_types"].count, 2)
        self.assertTrue(
            any("FlagCancel" in line and "field" in line for line in snap.metrics["pub_cancel_types"].offenders),
        )

    def test_pub_fn_configset_with_repo(self) -> None:
        repo_ref = {
            "borrowed_ref": {
                "is_mutable": False,
                "type": {"resolved_path": {"path": "Repository"}},
            }
        }
        config_ref = {
            "borrowed_ref": {
                "is_mutable": False,
                "type": {"resolved_path": {"path": "ConfigSet"}},
            }
        }
        payload = _minimal_rustdoc(
            {
                "name": "prepare",
                "inner": {
                    "function": {
                        "sig": {
                            "inputs": [["repo", repo_ref], ["cfg", config_ref]],
                            "output": None,
                        },
                        "header": {},
                    }
                },
            }
        )
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_fn_configset_with_repo"].count, 1)

    def test_pub_repo_constructors(self) -> None:
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [10, 11]}},
                },
                "10": {
                    "id": 10,
                    "crate_id": 0,
                    "visibility": "public",
                    "inner": {
                        "impl": {
                            "items": [11],
                            "for": {"resolved_path": {"path": "Repository"}},
                        }
                    },
                },
                "11": {
                    "id": 11,
                    "crate_id": 0,
                    "name": "open",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {
                                "inputs": [],
                                "output": {
                                    "resolved_path": {
                                        "path": "Result",
                                        "args": {
                                            "angle_bracketed": {
                                                "args": [{"type": {"generic": "Self"}}]
                                            }
                                        },
                                    }
                                },
                            },
                            "header": {},
                        }
                    },
                },
            },
            "paths": {"0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"}},
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_repo_constructors"].count, 1)

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
