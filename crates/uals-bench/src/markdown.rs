//! Markdown tables for one or several reports. Only ids, ranks and numbers, no code.

use std::fmt::Write as _;

use crate::report::{Group, Report};

/// A millisecond value with two decimals, or `-` when missing.
fn opt(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"))
}

/// A rank, or `-` when there is none.
fn rank_cell(rank: Option<u32>) -> String {
    rank.map_or_else(|| "-".to_string(), |r| r.to_string())
}

/// The header of a quality table; latency columns only when `with_latency`.
fn quality_header(out: &mut String, with_latency: bool) {
    out.push_str("| run | n | failed | Hit@1 | Hit@5 | Hit@10 | MRR | MRR+sec | tokens |");
    out.push_str(if with_latency { " cold ms | warm p50 ms | warm p95 ms |\n" } else { "\n" });
    out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    out.push_str(if with_latency { "---:|---:|---:|\n" } else { "\n" });
}

/// One row of a quality table; the latency cells only when `latency` carries the report.
fn quality_row(out: &mut String, name: &str, g: &Group, latency: Option<&Report>) {
    let _ = write!(
        out,
        "| {name} | {} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.0} |",
        g.n,
        g.failed,
        g.primary.hit_at_1,
        g.primary.hit_at_5,
        g.primary.hit_at_10,
        g.primary.mrr,
        g.with_secondary.mrr,
        g.mean_tokens,
    );
    if let Some(report) = latency {
        let l = &report.summary.latency;
        let _ = write!(out, " {} | {} | {} |", opt(l.cold_ms), opt(l.warm_p50_ms), opt(l.warm_p95_ms));
    }
    out.push('\n');
}

/// Renders the reports side by side: overall, a table per query type and per half, usages,
/// and the rank of the first correct hit for every query.
pub fn render(reports: &[Report]) -> String {
    let mut out = String::from("# uals-bench report\n\n");
    out.push_str("Hit@k and MRR count a hit as correct when it matches a `primary` entry; `MRR+sec` also counts `secondary` ones. `tokens` is the mean size of the concise text an agent receives (characters / 4).\n\n");

    out.push_str("## Overall\n\n");
    quality_header(&mut out, true);
    for r in reports {
        quality_row(&mut out, &r.name, &r.summary.overall, Some(r));
    }

    let keys = |f: fn(&Report) -> Vec<String>| {
        let mut all: Vec<String> = reports.iter().flat_map(f).collect();
        all.sort();
        all.dedup();
        all
    };
    for kind in keys(|r| r.summary.by_type.keys().cloned().collect()) {
        let _ = write!(out, "\n## Type: {kind}\n\n");
        quality_header(&mut out, false);
        for r in reports {
            if let Some(g) = r.summary.by_type.get(&kind) {
                quality_row(&mut out, &r.name, g, None);
            }
        }
    }
    for half in keys(|r| r.summary.by_half.keys().cloned().collect()) {
        let _ = write!(out, "\n## Half: {half}\n\n");
        quality_header(&mut out, false);
        for r in reports {
            if let Some(g) = r.summary.by_half.get(&half) {
                quality_row(&mut out, &r.name, g, None);
            }
        }
    }

    if reports.iter().any(|r| r.summary.usages.is_some()) {
        out.push_str(
            "\n## Usages\n\nMean over the symbols whose call succeeded; R = recall, P = precision. `failed` counts symbols whose call failed; they are not in the means. `incomplete` counts averaged symbols whose answer was not complete: their recall is a lower bound.\n\n",
        );
        out.push_str(
            "| run | symbols | failed | incomplete | file R | file P | line R | line P |\n|---|---:|---:|---:|---:|---:|---:|---:|\n",
        );
        for r in reports {
            if let Some(u) = &r.summary.usages {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {:.3} | {:.3} | {:.3} | {:.3} |",
                    r.name,
                    u.symbols,
                    u.failed,
                    u.incomplete,
                    u.file_recall,
                    u.file_precision,
                    u.line_recall,
                    u.line_precision
                );
            }
        }
        for r in reports.iter().filter(|r| !r.usages.is_empty()) {
            let _ = write!(out, "\n### Usages per symbol: {}\n\n", r.name);
            out.push_str("| id | file R | file P | line R | line P | expected lines | found lines |\n|---|---:|---:|---:|---:|---:|---:|\n");
            for u in &r.usages {
                if u.failed {
                    let _ = writeln!(out, "| {} | err | err | err | err | - | - |", u.id);
                    continue;
                }
                let _ = writeln!(
                    out,
                    "| {}{} | {:.3} | {:.3} | {:.3} | {:.3} | {} | {} |",
                    u.id,
                    if u.incomplete { " (incomplete)" } else { "" },
                    u.file_recall,
                    u.file_precision,
                    u.line_recall,
                    u.line_precision,
                    u.expected_lines,
                    u.found_lines
                );
            }
        }
    }

    out.push_str("\n## Rank of the first correct hit per query\n\nCell is `primary/with secondary`; `-` means none in the top 30, `err` means the call failed.\n\n");
    out.push_str("| id | type | half |");
    for r in reports {
        let _ = write!(out, " {} |", r.name);
    }
    out.push_str("\n|---|---|---|");
    for _ in reports {
        out.push_str("---|");
    }
    out.push('\n');
    let mut ids: Vec<(&str, &str, &str)> = Vec::new();
    for r in reports {
        for q in &r.queries {
            if !ids.iter().any(|(id, _, _)| *id == q.id) {
                ids.push((&q.id, q.kind.as_str(), q.half.as_str()));
            }
        }
    }
    for (id, kind, half) in ids {
        let _ = write!(out, "| {id} | {kind} | {half} |");
        for r in reports {
            let cell = match r.queries.iter().find(|q| q.id == id) {
                None => "n/a".to_string(),
                Some(q) if q.failed => "err".to_string(),
                Some(q) => format!("{}/{}", rank_cell(q.rank_primary), rank_cell(q.rank_any)),
            };
            let _ = write!(out, " {cell} |");
        }
        out.push('\n');
    }
    out
}
