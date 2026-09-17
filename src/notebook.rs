//! nbformat 4.x model. Only what we touch is typed; everything else lives in
//! `extra` flatten maps so foreign metadata round-trips losslessly.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notebook {
    pub cells: Vec<Cell>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cell {
    pub cell_type: String,
    #[serde(with = "multiline")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<Value>>,
    /// Everything else (id, metadata, execution_count, ...) — kept as raw
    /// Values so null-vs-missing and foreign fields survive round-trips.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Cell {
    pub fn execution_count(&self) -> Option<i64> {
        self.extra.get("execution_count").and_then(Value::as_i64)
    }
}

impl Notebook {
    pub fn open(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Atomic save: write sibling temp file, then rename over the original.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        let tmp = path.with_extension("ipynb.tmp");
        std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming onto {}", path.display()))?;
        Ok(())
    }
}

/// nbformat "multiline string": a string or a list of lines (each keeping its
/// trailing `\n`). We hold a plain String in memory and write the list form,
/// matching what jupyter tools emit.
mod multiline {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Src {
            One(String),
            Many(Vec<String>),
        }
        Ok(match Src::deserialize(d)? {
            Src::One(s) => s,
            Src::Many(v) => v.concat(),
        })
    }

    pub fn serialize<S: Serializer>(s: &str, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_seq(s.split_inclusive('\n'))
    }
}

/// Join a multiline-string `Value` (string or list of strings) found inside
/// outputs, without needing typed output structs.
pub fn join_multiline(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical jupyter-written form: list sources, cell ids, unknown metadata.
    const NB: &str = r##"{
      "cells": [
        {
          "cell_type": "markdown",
          "id": "abc-123",
          "metadata": {"collapsed": false, "someToolField": [1, 2]},
          "source": ["# Title\n", "\n", "text"]
        },
        {
          "cell_type": "code",
          "execution_count": 2,
          "id": "def-456",
          "metadata": {},
          "outputs": [
            {"name": "stdout", "output_type": "stream", "text": ["hi\n"]},
            {"data": {"text/plain": ["42"]}, "execution_count": 2, "metadata": {}, "output_type": "execute_result"}
          ],
          "source": ["x = 42\n", "print(\"hi\")\n", "x"]
        },
        {
          "cell_type": "code",
          "execution_count": null,
          "id": "ghi-789",
          "metadata": {},
          "outputs": [],
          "source": []
        }
      ],
      "metadata": {"kernelspec": {"display_name": "phy", "language": "python", "name": "phy"}},
      "nbformat": 4,
      "nbformat_minor": 5
    }"##;

    #[test]
    fn roundtrip_is_lossless() {
        let nb: Notebook = serde_json::from_str(NB).unwrap();
        let orig: Value = serde_json::from_str(NB).unwrap();
        assert_eq!(serde_json::to_value(&nb).unwrap(), orig);
    }

    #[test]
    fn source_concatenates() {
        let nb: Notebook = serde_json::from_str(NB).unwrap();
        assert_eq!(nb.cells[0].source, "# Title\n\ntext");
        assert_eq!(nb.cells[1].execution_count(), Some(2));
        assert_eq!(nb.cells[2].execution_count(), None); // null in the JSON
        assert_eq!(nb.cells[2].source, "");
    }

    #[test]
    fn string_form_source_accepted() {
        let cell: Cell =
            serde_json::from_str(r#"{"cell_type": "markdown", "metadata": {}, "source": "a\nb"}"#)
                .unwrap();
        assert_eq!(cell.source, "a\nb");
        // and re-serializes in list form
        let v = serde_json::to_value(&cell).unwrap();
        assert_eq!(v["source"], serde_json::json!(["a\n", "b"]));
    }
}
