//! Data-preservation and grammar regressions for the editable notebook view.

use super::{MAX_NOTEBOOK_BYTES, Notebook};
use serde_json::{Value, json};
use std::collections::HashSet;

fn notebook_fixture() -> Value {
    json!({
        "nbformat": 4,
        "nbformat_minor": 5,
        "metadata": {
            "kernelspec": {"name": "python3", "display_name": "Python 3"},
            "custom": {"keep": [null, true, "λ"]}
        },
        "unrelated_top_level": {"number": 18446744073709551615_u64},
        "cells": [
            {
                "id": "original-code",
                "cell_type": "code",
                "source": [
                    "value = 7\r\n",
                    "# %% [markdown] cell:7\r\n",
                    "# %%% [raw] cell:8\r\n",
                    "last line without newline"
                ],
                "metadata": {"tags": ["retain"], "custom": {"trusted": false}},
                "execution_count": 17,
                "outputs": [
                    {"output_type": "stream", "name": "stdout", "text": ["saved 7\n"]},
                    {"output_type": "display_data", "metadata": {"keep": true},
                     "data": {"text/plain": "saved display", "image/png": "AQIDBA=="}}
                ],
                "unrelated_cell_field": {"keep": "code"}
            },
            {
                "id": "original-markdown",
                "cell_type": "markdown",
                "source": "# Original heading\n\n",
                "metadata": {"tags": ["documentation"]},
                "attachments": {"diagram.png": {"image/png": "AQID"}},
                "unrelated_cell_field": {"keep": "markdown"}
            },
            {
                "id": "original-raw",
                "cell_type": "raw",
                "source": ["raw unchanged"],
                "metadata": {"raw_mimetype": "text/plain"}
            }
        ]
    })
}

fn decode(original: &Value) -> Notebook {
    Notebook::parse(&original.to_string(), "fixture.ipynb").expect("valid notebook fixture")
}

fn save(notebook: &Notebook, edited: &str) -> Value {
    serde_json::from_str(
        &notebook
            .serialize_edited(edited)
            .expect("valid editable notebook"),
    )
    .expect("serializer returns notebook JSON")
}

#[test]
fn unchanged_round_trip_preserves_complete_native_document_and_literal_markers() {
    let original = notebook_fixture();
    let notebook = decode(&original);
    let editable = notebook.editable_text().unwrap();
    assert!(editable.starts_with("# %% [code] cell:0\n"));
    assert!(editable.contains("# %%% [markdown] cell:7\n"));
    assert!(editable.contains("# %%%% [raw] cell:8\n"));
    assert!(!editable.contains("saved display"));
    assert!(!editable.contains("AQIDBA=="));
    let saved = save(&notebook, &editable);
    assert_eq!(saved, original);
    assert_eq!(decode(&saved).editable_text().unwrap(), editable);
}

#[test]
fn cell_boundaries_keep_empty_sources_line_endings_and_source_array_shape() {
    for source in [
        "",
        "\n",
        "\r\n",
        "\r",
        "one",
        "one\n",
        "one\n\n",
        "one\r\ntwo\r\n",
        "one\rtwo\r",
        "# %% [code] cell:9",
        "# %%% [markdown] cell:1\n",
        "# %%%% [raw]\r\n",
        "  # %% [code] cell:0\n",
        "\u{feff}literal BOM inside source",
        "λ and 🐍\n",
    ] {
        let original = json!({
            "nbformat": 4,
            "nbformat_minor": 5,
            "metadata": {},
            "cells": [
                {"id": "before", "cell_type": "raw", "metadata": {},
                 "source": ["arbitrary ", "array segmentation"]},
                {"id": "middle", "cell_type": "markdown", "metadata": {}, "source": source},
                {"id": "last", "cell_type": "raw", "metadata": {}, "source": [source]}
            ]
        });
        let notebook = decode(&original);
        let editable = notebook.editable_text().unwrap();
        assert_eq!(save(&notebook, &editable), original, "source {source:?}");
    }
}

#[test]
fn reordering_and_source_edits_preserve_identity_metadata_and_saved_outputs() {
    let original = notebook_fixture();
    let notebook = decode(&original);
    let edited = concat!(
        "# %% [markdown] cell:1\nRevised heading\n\n",
        "# %% [code] cell:0\nprint(42)\n\n",
        "# %% [raw] cell:2\nraw unchanged"
    );
    let saved = save(&notebook, edited);
    assert_eq!(saved["metadata"], original["metadata"]);
    assert_eq!(
        saved["unrelated_top_level"],
        original["unrelated_top_level"]
    );
    assert_eq!(saved["nbformat"], 4);
    assert_eq!(saved["nbformat_minor"], 5);
    assert_eq!(saved["cells"][0]["id"], "original-markdown");
    assert_eq!(saved["cells"][0]["source"], json!(["Revised heading\n"]));
    for field in ["metadata", "attachments", "unrelated_cell_field"] {
        assert_eq!(saved["cells"][0][field], original["cells"][1][field]);
    }
    assert_eq!(saved["cells"][1]["id"], "original-code");
    assert_eq!(saved["cells"][1]["source"], json!(["print(42)\n"]));
    for field in [
        "metadata",
        "outputs",
        "execution_count",
        "unrelated_cell_field",
    ] {
        assert_eq!(saved["cells"][1][field], original["cells"][0][field]);
    }
    assert_eq!(saved["cells"][2], original["cells"][2]);
}

#[test]
fn changing_cell_type_changes_only_type_source_and_code_execution_fields() {
    let original = notebook_fixture();
    let saved = save(
        &decode(&original),
        "# %% [markdown] cell:0\nExplain the result\n# %% [code] cell:1\nprint('new code')",
    );
    let prose = &saved["cells"][0];
    assert_eq!(prose["id"], "original-code");
    assert_eq!(prose["cell_type"], "markdown");
    assert_eq!(prose["metadata"], original["cells"][0]["metadata"]);
    assert_eq!(
        prose["unrelated_cell_field"],
        original["cells"][0]["unrelated_cell_field"]
    );
    assert!(prose.get("outputs").is_none());
    assert!(prose.get("execution_count").is_none());
    let code = &saved["cells"][1];
    assert_eq!(code["id"], "original-markdown");
    assert_eq!(code["cell_type"], "code");
    assert_eq!(code["metadata"], original["cells"][1]["metadata"]);
    assert_eq!(code["attachments"], original["cells"][1]["attachments"]);
    assert_eq!(code["execution_count"], Value::Null);
    assert_eq!(code["outputs"], json!([]));
}

#[test]
fn repeated_missing_and_out_of_range_indices_create_distinct_fresh_cells() {
    let original = notebook_fixture();
    let saved = save(
        &decode(&original),
        concat!(
            "# %% [code] cell:0\nfirst\n",
            "# %% [code] cell:0\nsecond\n",
            "# %% [raw] cell:999\nthird\n",
            "# %% [markdown]\nfourth"
        ),
    );
    let cells = saved["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 4);
    assert_eq!(cells[0]["id"], "original-code");
    assert_eq!(cells[0]["outputs"], original["cells"][0]["outputs"]);
    let ids: HashSet<_> = cells
        .iter()
        .map(|cell| cell["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), cells.len());
    for cell in &cells[1..] {
        let id = cell["id"].as_str().unwrap();
        assert!(!id.is_empty() && id.len() <= 64);
        assert!(
            id.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        );
        assert!(!["original-code", "original-markdown", "original-raw"].contains(&id));
        assert_eq!(cell["metadata"], json!({}));
        assert!(cell.get("unrelated_cell_field").is_none());
    }
    assert_eq!(cells[1]["execution_count"], Value::Null);
    assert_eq!(cells[1]["outputs"], json!([]));
    assert!(cells[2].get("outputs").is_none());
    assert!(cells[3].get("execution_count").is_none());
}

#[test]
fn non_marker_preamble_is_rejected_and_empty_view_intentionally_removes_cells() {
    let original = notebook_fixture();
    let notebook = decode(&original);
    for invalid in [
        "prose before a marker\n# %% [code] cell:0\nx",
        "\n# %% [code] cell:0\nx",
        " # %% [code] cell:0\nx",
        "# %% [unknown] cell:0\nx",
        "# %% [code] cell:-1\nx",
    ] {
        assert!(notebook.serialize_edited(invalid).is_err(), "{invalid:?}");
    }
    let empty = save(&notebook, "");
    assert_eq!(empty["cells"], json!([]));
    assert_eq!(empty["metadata"], original["metadata"]);
    assert_eq!(
        empty["unrelated_top_level"],
        original["unrelated_top_level"]
    );
}

#[test]
fn malformed_notebooks_never_silently_discard_native_source() {
    for invalid in [
        "not JSON".to_string(),
        "[]".to_string(),
        json!({"nbformat": 4}).to_string(),
        json!({"nbformat": 4, "cells": {}}).to_string(),
        json!({"nbformat": 3, "cells": []}).to_string(),
        json!({"nbformat": 5, "cells": []}).to_string(),
        json!({"nbformat": 4, "cells": [null]}).to_string(),
        json!({"nbformat": 4, "cells": [{"cell_type": "future", "source": "keep"}]}).to_string(),
        json!({"nbformat": 4, "cells": [{"cell_type": "code", "source": 7}]}).to_string(),
        json!({"nbformat": 4, "cells": [{"cell_type": "code", "source": ["keep", 7]}]}).to_string(),
        json!({"nbformat": 4, "cells": [{"cell_type": "markdown", "source": {"keep": true}}]})
            .to_string(),
    ] {
        assert!(
            Notebook::parse(&invalid, "malformed.ipynb").is_err(),
            "{invalid}"
        );
    }
    let mut invalid_tail = notebook_fixture();
    invalid_tail["cells"][2]["source"] = json!(["readable first source", false]);
    assert!(Notebook::parse(&invalid_tail.to_string(), "malformed-tail.ipynb").is_err());
}

#[test]
fn complete_input_and_json_escape_expansion_have_independent_byte_bounds() {
    let oversized = json!({
        "nbformat": 4,
        "cells": [],
        "metadata": {"retained": "x".repeat(MAX_NOTEBOOK_BYTES)}
    })
    .to_string();
    let error = Notebook::parse(&oversized, "large.ipynb")
        .err()
        .expect("the complete JSON is bounded before decoding");
    assert!(error.to_string().contains("10 MiB"));
    let notebook = decode(&json!({"nbformat": 4, "cells": [], "metadata": {}}));
    let edited = format!("# %% [code]\n{}", "\u{0001}".repeat(MAX_NOTEBOOK_BYTES / 4));
    assert!(edited.len() < MAX_NOTEBOOK_BYTES);
    let error = notebook
        .serialize_edited(&edited)
        .expect_err("JSON escaping must not expand into an unbounded output allocation");
    assert!(error.to_string().contains("serialized notebook"));
}
