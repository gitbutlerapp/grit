"""Unit tests for scripts/api_ratchet.py."""
from __future__ import annotations

import io
import json
import sys
import tempfile
import textwrap
import tomllib
import unittest
from pathlib import Path
from unittest import mock

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
        """Spot-check that the migration ratchet still runs; baseline is the source of truth."""
        count = ar.count_error_message_construction_sites()
        baseline = tomllib.loads(ar.BASELINE_PATH.read_text(encoding="utf-8"))
        expected = int(baseline["counts"]["error_message_construction_sites"])
        self.assertEqual(count, expected)

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

    def test_variant_field_types_modern_tuple_encoding(self) -> None:
        """rustdoc encodes `Message(String)` as kind: {\"tuple\": [field_id]}."""
        index = {
            "10": {
                "id": 10,
                "inner": {
                    "struct_field": {
                        "name": "0",
                        "type": {"primitive": "str"},
                    }
                },
            },
            "11": {
                "id": 11,
                "name": "Message",
                "inner": {
                    "variant": {
                        "kind": {"tuple": [10]},
                    }
                },
            },
        }
        variant = index["11"]
        types = ar.variant_field_types(variant, index)
        self.assertEqual([ar.type_display(t) for t in types], ["str"])
        self.assertTrue(ar.variant_carries_catch_all_string(variant, index))
        self.assertTrue(ar.variant_is_string_tuple_payload(variant, index))

    def test_legacy_inline_tuple_string_payload(self) -> None:
        index = {
            "30": {
                "id": 30,
                "name": "Message",
                "inner": {
                    "variant": {
                        "kind": "tuple",
                        "fields": [{"type": {"primitive": "str"}}],
                    }
                },
            },
        }
        variant = index["30"]
        self.assertTrue(ar.variant_is_tuple_kind(variant))
        self.assertTrue(ar.variant_is_string_tuple_payload(variant, index))

    def test_catch_all_struct_field_not_tuple(self) -> None:
        index = {
            "20": {
                "id": 20,
                "name": "Message",
                "inner": {
                    "variant": {
                        "kind": "struct",
                        "fields": [
                            {
                                "name": "detail",
                                "type": {"primitive": "str"},
                            }
                        ],
                    }
                },
            },
        }
        variant = index["20"]
        self.assertTrue(ar.variant_carries_catch_all_string(variant, index))
        self.assertFalse(ar.variant_is_string_tuple_payload(variant, index))

    def test_error_ratchet_metrics_on_synthetic_message_enum(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "Error",
                "inner": {
                    "enum": {
                        "variants": [{"id": 2}],
                        "has_stripped_fields": False,
                    }
                },
            },
            {
                "id": 2,
                "name": "Message",
                "inner": {
                    "variant": {
                        "kind": {"tuple": [3]},
                    }
                },
            },
            {
                "id": 3,
                "inner": {
                    "struct_field": {
                        "name": "0",
                        "type": {"primitive": "str"},
                    }
                },
            },
        )
        payload["paths"]["1"] = {
            "crate_id": 0,
            "path": ["grit_lib", "error", "Error"],
            "kind": "enum",
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["umbrella_error_variants"].count, 1)
        self.assertEqual(snap.metrics["pub_error_catch_all_variants"].count, 1)
        self.assertEqual(snap.metrics["pub_error_string_tuple_variants"].count, 1)

    def test_umbrella_wraps_metric_on_synthetic_enum(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "RevParseError",
                "inner": {
                    "enum": {
                        "variants": [{"id": 2}],
                        "has_stripped_fields": False,
                    }
                },
            },
            {
                "id": 2,
                "name": "Wrapped",
                "inner": {
                    "variant": {
                        "kind": {"tuple": [3]},
                    }
                },
            },
            {
                "id": 3,
                "inner": {
                    "struct_field": {
                        "name": "0",
                        "type": {
                            "resolved_path": {
                                "path": "grit_lib::error::Error",
                                "id": 99,
                                "args": {"angle_bracketed": {"args": []}},
                            }
                        },
                    }
                },
            },
        )
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_area_error_wraps_umbrella"].count, 1)

    def _area_enum_with_field_types(self, *field_types: dict) -> dict:
        """Synthetic ``AreaError`` whose struct variants each hold one field of ``field_types``."""
        variant_ids = list(range(2, 2 + 2 * len(field_types), 2))
        items: list[dict] = [
            {
                "name": "AreaError",
                "attrs": ["#[non_exhaustive]"],
                "inner": {"enum": {"variants": variant_ids, "has_stripped_fields": False}},
            }
        ]
        for n, fty in enumerate(field_types):
            vid = variant_ids[n]
            items.append(
                {
                    "id": vid,
                    "name": f"V{n}",
                    "inner": {
                        "variant": {
                            "kind": {"struct": {"fields": [vid + 1], "has_stripped_fields": False}}
                        }
                    },
                }
            )
            items.append({"id": vid + 1, "name": "source", "inner": {"struct_field": fty}})
        payload = _minimal_rustdoc(*items)
        payload["paths"]["99"] = {
            "crate_id": 0,
            "path": ["grit_lib", "error", "Error"],
            "kind": "enum",
        }
        payload["paths"]["98"] = {"crate_id": 1, "path": ["std", "io", "error", "Error"], "kind": "struct"}
        payload["paths"]["97"] = {"crate_id": 0, "path": ["grit_lib", "index", "IndexError"], "kind": "enum"}
        return payload

    @staticmethod
    def _path(path: str, rid: int, *args: dict) -> dict:
        return {
            "resolved_path": {
                "path": path,
                "id": rid,
                "args": {"angle_bracketed": {"args": [{"type": a} for a in args], "constraints": []}},
            }
        }

    def test_umbrella_wraps_ignores_io_and_other_area_errors(self) -> None:
        io_error = self._path("std::io::Error", 98)
        boxed_area = self._path("Box", 500, self._path("IndexError", 97))
        payload = self._area_enum_with_field_types(io_error, boxed_area)
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_area_error_wraps_umbrella"].count, 0)

    def test_umbrella_wraps_finds_umbrella_in_struct_fields_and_generics(self) -> None:
        umbrella = self._path("Error", 99)
        boxed = self._path("Box", 500, umbrella)
        listed = self._path("Vec", 501, {"tuple": [self._path("String", 502), umbrella]})
        payload = self._area_enum_with_field_types(boxed, listed)
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_area_error_wraps_umbrella"].count, 2)

    def test_error_message_construction_sites_synthetic_tree(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "sample.rs"
            src.parent.mkdir(parents=True)
            src.write_text(
                'fn f() { let _ = Error::Message("a"); Error::Message(format!("b")); }',
                encoding="utf-8",
            )
            with mock.patch.object(ar, "GRIT_LIB", root):
                self.assertEqual(ar.count_error_message_construction_sites(), 2)

    def test_write_baseline_preserves_semver_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "api-ratchet.toml"
            path.write_text(
                textwrap.dedent(
                    """
                    [meta]
                    updated = "2026-01-01"
                    semver_baseline = "deadbeef"

                    [counts]
                    public_api_lines = 1
                    """
                ).strip()
                + "\n",
                encoding="utf-8",
            )
            snap = ar.RatchetSnapshot(
                metrics={key: ar.MetricResult(0) for key in ar.METRIC_KEYS}
            )
            snap.metrics["public_api_lines"] = ar.MetricResult(2)
            ar.write_baseline(path, snap)
            text = path.read_text(encoding="utf-8")
            self.assertIn('semver_baseline = "deadbeef"', text)
            self.assertIn("public_api_lines = 2", text)

    def test_non_exhaustive_error_enum_excluded_when_attributed(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "SampleError",
                "inner": {
                    "enum": {
                        "variants": [],
                        "has_stripped_fields": False,
                    }
                },
            },
        )
        payload["index"]["1"]["attrs"] = {"non_exhaustive": None}
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_error_enum_not_non_exhaustive"].count, 0)

    def test_non_exhaustive_error_enum_included_without_attribute(self) -> None:
        payload = _minimal_rustdoc(
            {
                "name": "SampleError",
                "inner": {
                    "enum": {
                        "variants": [],
                        "has_stripped_fields": False,
                    }
                },
            },
        )
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertEqual(snap.metrics["pub_error_enum_not_non_exhaustive"].count, 1)


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

    def test_is_cancel_surface_type_recognizes_dyn_cancel_trait(self) -> None:
        index = {
            "9": {
                "id": 9,
                "name": "Cancel",
                "visibility": "public",
                "inner": {"trait": {"items": [], "bounds": []}},
            }
        }
        param_ty = {
            "borrowed_ref": {
                "is_mutable": False,
                "type": {
                    "dyn_trait": {
                        "traits": [{"trait": {"path": "Cancel", "id": 9, "args": None}}],
                    }
                },
            }
        }
        self.assertTrue(ar.is_cancel_surface_type(param_ty, index))

    def test_is_cancel_surface_type_recognizes_cancel_token_ref(self) -> None:
        index = {
            "9": {
                "id": 9,
                "name": "CancelToken",
                "visibility": "public",
                "inner": {
                    "struct": {
                        "kind": "unit",
                        "generics": {"params": [], "where_predicates": []},
                    }
                },
            }
        }
        param_ty = {
            "borrowed_ref": {
                "is_mutable": False,
                "type": {"resolved_path": {"path": "CancelToken", "id": 9, "args": None}},
            }
        }
        self.assertTrue(ar.is_cancel_surface_type(param_ty, index))

    def test_pub_cancel_types_cancel_token_param(self) -> None:
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"is_stripped": False, "items": [1, 2]}},
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "CancelToken",
                    "visibility": "public",
                    "inner": {
                        "struct": {
                            "kind": "unit",
                            "generics": {"params": [], "where_predicates": []},
                        }
                    },
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "name": "run",
                    "visibility": "public",
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
                                                    "resolved_path": {
                                                        "path": "CancelToken",
                                                        "id": 1,
                                                        "args": None,
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
            },
            "paths": {
                "0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
                "1": {
                    "crate_id": 0,
                    "path": ["grit_lib", "progress", "CancelToken"],
                    "kind": "struct",
                },
                "2": {
                    "crate_id": 0,
                    "path": ["grit_lib", "run"],
                    "kind": "function",
                },
            },
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertGreaterEqual(snap.metrics["pub_cancel_types"].count, 2)
        offenders = snap.metrics["pub_cancel_types"].offenders
        self.assertTrue(any("CancelToken" in line for line in offenders))
        self.assertTrue(any("run(cancel)" in line for line in offenders))

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

    def test_pub_cancel_types_cancel_token_struct_field(self) -> None:
        cancel_token_ref = {
            "borrowed_ref": {
                "is_mutable": False,
                "type": {"resolved_path": {"path": "CancelToken", "id": 1, "args": None}},
            }
        }
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": [2, 3, 4]}},
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "CancelToken",
                    "visibility": "public",
                    "inner": {
                        "struct": {
                            "kind": "unit",
                            "generics": {"params": [], "where_predicates": []},
                        }
                    },
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "name": "RunOptions",
                    "visibility": "public",
                    "inner": {
                        "struct": {
                            "kind": {"plain": {"fields": [3], "has_stripped_fields": False}},
                            "generics": {"params": [], "where_predicates": []},
                        }
                    },
                },
                "3": {
                    "id": 3,
                    "crate_id": 0,
                    "name": "cancel",
                    "visibility": "public",
                    "inner": {"struct_field": cancel_token_ref},
                },
                "4": {
                    "id": 4,
                    "crate_id": 0,
                    "name": "run",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {"inputs": [], "output": None},
                            "header": {},
                        }
                    },
                },
            },
            "paths": {
                "0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
                "1": {
                    "crate_id": 0,
                    "path": ["grit_lib", "progress", "CancelToken"],
                    "kind": "struct",
                },
                "2": {
                    "crate_id": 0,
                    "path": ["grit_lib", "RunOptions"],
                    "kind": "struct",
                },
                "3": {
                    "crate_id": 0,
                    "path": ["grit_lib", "RunOptions", "cancel"],
                    "kind": "structfield",
                },
                "4": {"crate_id": 0, "path": ["grit_lib", "run"], "kind": "function"},
            },
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        self.assertGreaterEqual(snap.metrics["pub_cancel_types"].count, 2)
        offenders = snap.metrics["pub_cancel_types"].offenders
        self.assertTrue(any("CancelToken" in line for line in offenders))
        self.assertTrue(any("field cancel" in line for line in offenders))

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
        self.assertEqual(snap.metrics["pub_repo_constructors"].count, 0)
        self.assertEqual(snap.metrics["pub_repo_constructor_offenders"].count, 1)

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


def _free_fn(name: str, inputs: list | None = None) -> dict:
    return {
        "name": name,
        "inner": {
            "function": {
                "sig": {"inputs": inputs or [], "output": None},
                "header": {},
            }
        },
    }


REPO_REF = {
    "borrowed_ref": {
        "is_mutable": False,
        "type": {"resolved_path": {"path": "Repository"}},
    }
}
SELF_REF = {"borrowed_ref": {"is_mutable": False, "type": {"generic": "Self"}}}


class WithInNameTests(unittest.TestCase):
    def test_variant_names_count_and_builder_setters_do_not(self) -> None:
        self.assertTrue(ar.has_with_in_name("open_with_options"))
        self.assertTrue(ar.has_with_in_name("diff_trees_with_renames"))
        self.assertFalse(ar.has_with_in_name("with_depth"))
        self.assertFalse(ar.has_with_in_name("_with_leading_underscore"))
        self.assertFalse(ar.has_with_in_name("without_renames"))

    def test_options_builder_setter_not_counted(self) -> None:
        payload = {
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "inner": {"module": {"items": []}},
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "FetchOptions",
                    "visibility": "public",
                    "inner": {"struct": {"kind": {"plain": {"fields": []}}, "impls": [2]}},
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "visibility": "public",
                    "inner": {
                        "impl": {
                            "items": [3, 4],
                            "for": {"resolved_path": {"path": "FetchOptions", "id": 1}},
                        }
                    },
                },
                "3": {
                    "id": 3,
                    "crate_id": 0,
                    "name": "with_depth",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {"inputs": [["self", {"generic": "Self"}]], "output": None},
                            "header": {},
                        }
                    },
                },
                "4": {
                    "id": 4,
                    "crate_id": 0,
                    "name": "fetch_with_depth",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {"inputs": [["self", {"generic": "Self"}]], "output": None},
                            "header": {},
                        }
                    },
                },
            },
            "paths": {
                "0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
                "1": {"crate_id": 0, "path": ["grit_lib", "remote", "FetchOptions"], "kind": "struct"},
            },
        }
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        metric = snap.metrics["pub_fn_with_in_name"]
        self.assertEqual(metric.count, 1)
        self.assertIn("fetch_with_depth", metric.offenders[0])
        self.assertTrue(metric.offenders[0].startswith("grit_lib::remote::FetchOptions ("))
        self.assertEqual(metric.modules, ["remote"])


class ModuleResolverTests(unittest.TestCase):
    def test_top_module_of_path(self) -> None:
        self.assertEqual(ar.top_module_of_path("grit_lib::diff::core::f"), "diff")
        self.assertEqual(ar.top_module_of_path("grit_lib::f"), ar.ROOT_MODULE_LABEL)
        self.assertIsNone(ar.top_module_of_path("std::fs::read"))

    def test_public_path_wins_over_defining_path(self) -> None:
        payload = {
            "index": {"5": {"id": 5, "crate_id": 0, "name": "f", "inner": {}}},
            "paths": {"5": {"crate_id": 0, "path": ["grit_lib", "rev_parse", "f"]}},
        }
        resolver = ar.ModuleResolver(
            payload,
            {"5": ["grit_lib::revwalk::rev_parse::f", "grit_lib::prelude::f"]},
            canonical={},
        )
        self.assertEqual(resolver.module_of("5"), "revwalk")

    def test_defining_path_goes_through_canonical_map(self) -> None:
        payload = {
            "index": {"5": {"id": 5, "crate_id": 0, "name": "f", "inner": {}}},
            "paths": {"5": {"crate_id": 0, "path": ["grit_lib", "rev_parse", "f"]}},
        }
        resolver = ar.ModuleResolver(payload, {}, canonical={"rev_parse": "revwalk"})
        self.assertEqual(resolver.module_of("5"), "revwalk")

    def test_glob_reexport_of_private_module(self) -> None:
        payload = {
            "index": {
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "history",
                    "visibility": "public",
                    "inner": {"module": {"items": [2]}},
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "visibility": "public",
                    "inner": {
                        "use": {
                            "source": "crate::history_root",
                            "name": "history_root",
                            "id": 3,
                            "is_glob": True,
                        }
                    },
                },
                "4": {"id": 4, "crate_id": 0, "name": "reword", "inner": {}},
            },
            "paths": {
                "1": {"crate_id": 0, "path": ["grit_lib", "revwalk", "history"]},
                "4": {"crate_id": 0, "path": ["grit_lib", "history_root", "reword"]},
            },
        }
        resolver = ar.ModuleResolver(payload, {}, canonical={})
        self.assertEqual(resolver.module_of("4"), "revwalk")

    def test_method_resolves_through_owner_type(self) -> None:
        payload = {
            "index": {
                "1": {"id": 1, "crate_id": 0, "name": "Diff", "inner": {"struct": {}}},
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "inner": {"impl": {"items": [3], "for": {"resolved_path": {"path": "Diff", "id": 1}}}},
                },
                "3": {"id": 3, "crate_id": 0, "name": "trees", "inner": {"function": {}}},
            },
            "paths": {"1": {"crate_id": 0, "path": ["grit_lib", "diff", "handle", "Diff"]}},
        }
        resolver = ar.ModuleResolver(payload, {}, canonical={})
        self.assertEqual(resolver.module_of("3"), "diff")

    def test_span_fallback_and_unknown(self) -> None:
        payload = {
            "index": {
                "7": {
                    "id": 7,
                    "crate_id": 0,
                    "name": "g",
                    "span": {"filename": "grit-lib/src/odb/store/files.rs", "begin": [1, 1]},
                    "inner": {},
                },
                "8": {"id": 8, "crate_id": 0, "name": "h", "inner": {}},
            },
            "paths": {},
        }
        resolver = ar.ModuleResolver(payload, {}, canonical={"odb": "objects"})
        self.assertEqual(resolver.module_of("7"), "objects")
        self.assertEqual(resolver.module_of("8"), ar.UNKNOWN_MODULE_LABEL)

    def test_canonical_map_loads_top_segment(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "canonical-modules.toml"
            path.write_text('[map]\nodb = "objects::odb"\ndiff = "diff"\n', encoding="utf-8")
            self.assertEqual(
                ar.load_canonical_module_map(path),
                {"odb": "objects", "diff": "diff"},
            )


def _report_snapshot() -> ar.RatchetSnapshot:
    metrics = {key: ar.MetricResult(0) for key in ar.METRIC_KEYS}
    metrics["pub_fn_with_in_name"] = ar.MetricResult(
        2,
        ["grit_lib::diff::b_with_x :: b_with_x", "grit_lib::diff::a_with_x :: a_with_x"],
        ["diff", "diff"],
    )
    metrics["pub_free_fn_repository_first"] = ar.MetricResult(
        3,
        [
            "grit_lib::notes::read :: read",
            "grit_lib::porcelain::commit :: commit",
            "grit_lib::porcelain::status :: status",
        ],
        ["notes", "porcelain", "porcelain"],
    )
    metrics["pub_free_fn_git_dir_path_first"] = ar.MetricResult(
        1,
        ["grit_lib::diff::open :: open"],
        ["diff"],
    )
    return ar.RatchetSnapshot(metrics=metrics)


class ReportTests(unittest.TestCase):
    def test_group_offenders_by_module(self) -> None:
        grouped = ar.group_offenders_by_module(_report_snapshot())
        self.assertEqual(set(grouped), {"diff", "notes", "porcelain"})
        self.assertEqual(
            grouped["diff"]["pub_fn_with_in_name"],
            ["grit_lib::diff::a_with_x :: a_with_x", "grit_lib::diff::b_with_x :: b_with_x"],
        )
        self.assertEqual(len(grouped["porcelain"]["pub_free_fn_repository_first"]), 2)

    def test_offenders_without_modules_group_as_unknown(self) -> None:
        snap = _report_snapshot()
        snap.metrics["pub_fn_two_plus_bare_bool"] = ar.MetricResult(1, ["x :: x"])
        grouped = ar.group_offenders_by_module(snap)
        self.assertEqual(grouped[ar.UNKNOWN_MODULE_LABEL]["pub_fn_two_plus_bare_bool"], ["x :: x"])

    def test_render_report_orders_modules_by_offender_count(self) -> None:
        text = ar.render_report(_report_snapshot())
        self.assertIn("api-ratchet report: 6 offenders in 3 modules", text)
        self.assertIn("  pub_free_fn_repository_first = 3", text)
        headers = [line for line in text.splitlines() if line.startswith("## ")]
        self.assertEqual(
            headers,
            [
                "## diff (3: pub_fn_with_in_name 2, pub_free_fn_git_dir_path_first 1)",
                "## porcelain (2: pub_free_fn_repository_first 2)",
                "## notes (1: pub_free_fn_repository_first 1)",
            ],
        )
        self.assertIn("    grit_lib::diff::a_with_x :: a_with_x", text)

    def test_render_report_module_filter(self) -> None:
        text = ar.render_report(_report_snapshot(), module="porcelain")
        self.assertIn("2 offenders in module `porcelain`", text)
        self.assertNotIn("## diff", text)
        self.assertIn("  pub_fn_with_in_name = 0", text)
        empty = ar.render_report(_report_snapshot(), module="bundle")
        self.assertIn("No offenders in module `bundle`.", empty)

    def test_report_command_uses_rustdoc_and_filters(self) -> None:
        payload = _minimal_rustdoc(_free_fn("run", [["repo", REPO_REF]]))
        payload["paths"]["1"]["path"] = ["grit_lib", "notes", "run"]
        payload["index"]["0"]["inner"]["module"]["items"] = []
        with tempfile.TemporaryDirectory() as tmp:
            api_txt = Path(tmp) / "public-api.txt"
            api_txt.write_text("pub fn grit_lib::notes::run()\n", encoding="utf-8")
            out = io.StringIO()
            with (
                mock.patch.object(ar, "parse_pins", return_value={"nightly_date": "x"}),
                mock.patch.object(ar, "ensure_inputs"),
                mock.patch.object(ar, "load_rustdoc", return_value=payload),
                mock.patch.object(ar, "PUBLIC_API_TXT", api_txt),
                mock.patch.object(sys, "stdout", out),
            ):
                self.assertEqual(ar.main(["report", "--module", "notes"]), 0)
        text = out.getvalue()
        self.assertIn("1 offenders in module `notes`", text)
        self.assertIn("grit_lib::notes::run :: run", text)

    def test_report_metric_keys_are_ratchet_metrics(self) -> None:
        for key in ar.REPORT_METRIC_KEYS:
            self.assertIn(key, ar.METRIC_KEYS)


class DocExampleFreeFnCallTests(unittest.TestCase):
    def test_rust_code_blocks_filters_languages_and_unhides_lines(self) -> None:
        docs = textwrap.dedent(
            """\
            Intro.

            ```
            # fn main() {
            let x = 1;
            #
            # }
            ```

            ```text
            not_rust(&repo)
            ```

            ```no_run,edition2021
            run(&repo);
            ```

            ```console
            $ grit log
            ```
            """
        )
        blocks = ar.rust_code_blocks(docs)
        self.assertEqual(len(blocks), 2)
        self.assertEqual(blocks[0].splitlines(), ["fn main() {", "let x = 1;", "}"])
        self.assertIn("run(&repo);", blocks[1])

    def test_count_repo_free_fn_calls(self) -> None:
        code = textwrap.dedent(
            """\
            use grit_lib::porcelain::commit::create_commit;
            fn create_commit(x: u8) {}
            let head = resolve_head(repo.git_dir())?;
            let _ = grit_lib::porcelain::commit::create_commit(&repo, &req)?;
            let reopened = Repository::open(repo.git_dir(), &opts)?;
            assert_eq!(reopened.git_dir(), repo.git_dir());
            let tree = repo.objects().read(&oid)?;
            let parsed = parse_tree(&tree.data)?;
            if (a) {}
            let refs = list_refs(
                &repo,
                Some(prefix(1)),
            )?;
            """
        )
        found = ar.count_repo_free_fn_calls(code, frozenset({"create_commit", "list_refs"}))
        self.assertEqual(
            found,
            ["resolve_head", "grit_lib::porcelain::commit::create_commit", "list_refs"],
        )

    def test_metric_scans_root_and_handle_docs_only(self) -> None:
        example = "```\nlet _ = run(&repo);\nlet _ = resolve_head(repo.git_dir());\n```"
        handle_return = {
            "resolved_path": {
                "path": "Objects",
                "id": 20,
                "args": {"angle_bracketed": {"args": [{"lifetime": "'_"}]}},
            }
        }
        plain_return = {"resolved_path": {"path": "Info", "id": 30, "args": None}}
        payload = {
            "root": 0,
            "index": {
                "0": {
                    "id": 0,
                    "crate_id": 0,
                    "name": "grit_lib",
                    "visibility": "public",
                    "docs": example,
                    "inner": {"module": {"items": [1, 5, 20, 30]}},
                },
                "1": {
                    "id": 1,
                    "crate_id": 0,
                    "name": "Repository",
                    "visibility": "public",
                    "docs": "No examples here.",
                    "inner": {"struct": {"kind": {"plain": {"fields": []}}, "impls": [2]}},
                },
                "2": {
                    "id": 2,
                    "crate_id": 0,
                    "inner": {
                        "impl": {
                            "items": [3, 4],
                            "for": {"resolved_path": {"path": "Repository", "id": 1}},
                        }
                    },
                },
                "3": {
                    "id": 3,
                    "crate_id": 0,
                    "name": "objects",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {"inputs": [["self", SELF_REF]], "output": handle_return},
                            "header": {},
                        }
                    },
                },
                "4": {
                    "id": 4,
                    "crate_id": 0,
                    "name": "info",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {"inputs": [["self", SELF_REF]], "output": plain_return},
                            "header": {},
                        }
                    },
                },
                "5": {
                    "id": 5,
                    "crate_id": 0,
                    "name": "run",
                    "visibility": "public",
                    "docs": example,
                    "inner": {
                        "function": {
                            "sig": {"inputs": [["repo", REPO_REF]], "output": None},
                            "header": {},
                        }
                    },
                },
                "20": {
                    "id": 20,
                    "crate_id": 0,
                    "name": "Objects",
                    "visibility": "public",
                    "inner": {"struct": {"kind": {"plain": {"fields": []}}, "impls": [21]}},
                },
                "21": {
                    "id": 21,
                    "crate_id": 0,
                    "inner": {
                        "impl": {
                            "items": [22],
                            "for": {"resolved_path": {"path": "Objects", "id": 20}},
                        }
                    },
                },
                "22": {
                    "id": 22,
                    "crate_id": 0,
                    "name": "read",
                    "visibility": "public",
                    "docs": "```\nrun(&repo);\n```",
                    "inner": {"function": {"sig": {"inputs": [], "output": None}, "header": {}}},
                },
                "30": {
                    "id": 30,
                    "crate_id": 0,
                    "name": "Info",
                    "visibility": "public",
                    "docs": example,
                    "inner": {"struct": {"kind": {"plain": {"fields": []}}, "impls": []}},
                },
            },
            "paths": {
                "0": {"crate_id": 0, "path": ["grit_lib"], "kind": "module"},
                "1": {"crate_id": 0, "path": ["grit_lib", "repo", "Repository"], "kind": "struct"},
                "5": {"crate_id": 0, "path": ["grit_lib", "run"], "kind": "function"},
                "20": {"crate_id": 0, "path": ["grit_lib", "objects", "Objects"], "kind": "struct"},
                "30": {"crate_id": 0, "path": ["grit_lib", "repo", "Info"], "kind": "struct"},
            },
        }
        self.assertEqual(
            ar.handle_type_ids(payload["index"], payload["paths"]),
            ["1", "20"],
        )
        snap = ar.compute_metrics(public_api_text="", payload=payload)
        metric = snap.metrics["doc_example_free_fn_calls"]
        self.assertEqual(
            sorted(metric.offenders),
            [
                "grit_lib :: resolve_head",
                "grit_lib :: run",
                "grit_lib::objects::Objects::read :: run",
            ],
        )
        self.assertEqual(metric.count, 3)


if __name__ == "__main__":
    unittest.main()
