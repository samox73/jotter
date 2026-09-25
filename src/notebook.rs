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

/// Lines kept per stream output; older ones are dropped (runaway print loops).
const MAX_STREAM_LINES: usize = 10_000;

impl Cell {
    pub fn execution_count(&self) -> Option<i64> {
        self.extra.get("execution_count").and_then(Value::as_i64)
    }

    /// Append a kernel output with nbformat stream semantics: consecutive
    /// same-name streams coalesce into one output (what jupyter frontends
    /// save), carriage returns overwrite the current line (tqdm), and the
    /// text is capped to its last MAX_STREAM_LINES lines.
    pub fn push_output(&mut self, output: Value) {
        let outputs = self.outputs.get_or_insert_with(Vec::new);
        let stream_name = |o: &Value| -> Option<String> {
            (o["output_type"] == "stream").then(|| o["name"].as_str().unwrap_or("").to_string())
        };
        let Some(name) = stream_name(&output) else {
            outputs.push(output);
            return;
        };
        let chunk = join_multiline(&output["text"]);
        match outputs.last_mut() {
            Some(last) if stream_name(last).as_deref() == Some(&name) => {
                let merged = collapse_cr(join_multiline(&last["text"]) + &chunk);
                last["text"] = cap_lines(merged, MAX_STREAM_LINES).into();
            }
            _ => {
                let mut output = output;
                output["text"] = cap_lines(collapse_cr(chunk), MAX_STREAM_LINES).into();
                outputs.push(output);
            }
        }
    }
}

/// `\r\n` is a newline; a bare `\r` returns to line start and later text
/// overwrites from there — terminal semantics, so `print(x, end="\r")` frames
/// stay visible and progress spam never accumulates in memory or saved JSON.
fn collapse_cr(s: String) -> String {
    if !s.contains('\r') {
        return s;
    }
    s.replace("\r\n", "\n")
        .split('\n')
        .map(|line| {
            let mut acc = String::new();
            for part in line.split('\r').filter(|p| !p.is_empty()) {
                let tail: String = acc.chars().skip(part.chars().count()).collect();
                acc = format!("{part}{tail}");
            }
            // a trailing \r is a *pending* overwrite: keep the marker so the
            // next merged chunk overwrites this line (renderers strip it)
            if line.ends_with('\r') {
                acc.push('\r');
            }
            acc
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keep only the last `max` lines, with a truncation marker up top.
fn cap_lines(s: String, max: usize) -> String {
    if s.len() < max {
        return s; // cannot have more lines than bytes
    }
    let extra = s.split('\n').count().saturating_sub(max);
    if extra == 0 {
        return s;
    }
    match s.match_indices('\n').nth(extra - 1) {
        Some((cut, _)) => format!("[... output truncated ...]\n{}", &s[cut + 1..]),
        None => s,
    }
}

impl Notebook {
    pub fn open(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut nb: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        nb.upgrade_to_4_5();
        Ok(nb)
    }

    /// nbformat 4.5 made cell ids mandatory (nbformat 5.1, Jan 2021). Like
    /// `nbformat.v4.upgrade`, give id-less cells a random id and bump the
    /// minor version, so every cell has a stable identity (nvim buffers key
    /// on it) and cells we add are valid. Only reaches disk on save.
    fn upgrade_to_4_5(&mut self) {
        if self.extra.get("nbformat").and_then(Value::as_i64) != Some(4) {
            return;
        }
        if self.extra.get("nbformat_minor").and_then(Value::as_i64) < Some(5) {
            self.extra.insert("nbformat_minor".into(), 5.into());
        }
        for cell in &mut self.cells {
            cell.extra
                .entry("id")
                .or_insert_with(|| new_cell_id().into());
        }
    }

    /// Atomic save: write sibling temp file, fsync, then rename over the
    /// original — a crash mid-save never leaves a corrupt notebook.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        // byte-identical to nbformat.write: sorted keys (serde_json::Map is a
        // BTreeMap; going through Value also sorts the typed Cell fields),
        // 1-space indent, trailing newline — no diff noise in git
        let mut buf = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
        serde::Serialize::serialize(
            &serde_json::to_value(self)?,
            &mut serde_json::Serializer::with_formatter(&mut buf, fmt),
        )?;
        let mut json = String::from_utf8(buf)?;
        json.push('\n');
        let tmp = path.with_extension("ipynb.tmp");
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(json.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, path).with_context(|| format!("renaming onto {}", path.display()))?;
        Ok(())
    }
}

/// nbformat cell id: 8 hex chars (schema: 1-64 of [a-zA-Z0-9-_]).
pub fn new_cell_id() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
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
    fn stream_outputs_coalesce_with_cr_semantics() {
        let mut cell: Cell =
            serde_json::from_str(r#"{"cell_type": "code", "metadata": {}, "source": ""}"#).unwrap();
        let stream = |name: &str, text: &str| serde_json::json!({"output_type": "stream", "name": name, "text": text});
        cell.push_output(stream("stdout", "10%\r"));
        cell.push_output(stream("stdout", "50%\r"));
        cell.push_output(stream("stdout", "100%\ndone\n"));
        cell.push_output(stream("stderr", "warn\n")); // different stream: own output
        cell.push_output(serde_json::json!({"output_type": "execute_result", "data": {}}));
        cell.push_output(stream("stdout", "after\n")); // not adjacent: not merged
        let outs = cell.outputs.as_ref().unwrap();
        assert_eq!(outs.len(), 4);
        assert_eq!(outs[0]["text"], "100%\ndone\n"); // \r frames overwritten
        assert_eq!(outs[1]["text"], "warn\n");
        assert_eq!(outs[3]["text"], "after\n");
    }

    #[test]
    fn stream_text_is_capped() {
        let long: String = "x\n".repeat(MAX_STREAM_LINES + 5);
        let capped = cap_lines(long, MAX_STREAM_LINES);
        assert!(capped.starts_with("[... output truncated ...]\n"));
        assert_eq!(capped.split('\n').count(), MAX_STREAM_LINES + 1);
        // crlf is a newline, not an overwrite
        assert_eq!(collapse_cr("a\r\nb".into()), "a\nb");
        // a trailing \r stays visible, with the pending-overwrite marker kept
        // so the next merged chunk replaces it (print(x, end="\\r") idiom)
        assert_eq!(collapse_cr("42%\r".into()), "42%\r");
        // overwrite is positional: a shorter frame leaves the tail behind
        assert_eq!(collapse_cr("12345\rab".into()), "ab345");
    }

    #[test]
    fn save_is_byte_identical_to_nbformat_write() {
        // json.dumps(nb, sort_keys=True, indent=1, ensure_ascii=False) + "\n"
        let canonical = "{\n \"cells\": [\n  {\n   \"cell_type\": \"code\",\n   \"execution_count\": null,\n   \"id\": \"a1\",\n   \"metadata\": {},\n   \"outputs\": [],\n   \"source\": [\n    \"x = 1\\n\",\n    \"é\"\n   ]\n  }\n ],\n \"metadata\": {},\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}\n";
        let path = std::env::temp_dir().join(format!("jotter-fmt-{}.ipynb", std::process::id()));
        std::fs::write(&path, canonical).unwrap();
        Notebook::open(&path).unwrap().save(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(saved, canonical);
    }

    #[test]
    fn pre_4_5_notebooks_upgrade_with_cell_ids() {
        let mut nb: Notebook = serde_json::from_str(
            r#"{"cells": [{"cell_type": "code", "metadata": {}, "source": "", "outputs": []},
                          {"cell_type": "markdown", "id": "keep", "metadata": {}, "source": ""}],
                "metadata": {}, "nbformat": 4, "nbformat_minor": 4}"#,
        )
        .unwrap();
        nb.upgrade_to_4_5();
        assert_eq!(nb.extra["nbformat_minor"], 5);
        assert!(nb.cells[0].extra["id"].as_str().is_some_and(|id| id.len() == 8));
        assert_eq!(nb.cells[1].extra["id"], "keep");
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
