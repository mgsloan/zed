use rope::Rope;
use std::fmt;
use std::{fmt::Display, ops::Range, path::Path};

pub struct SettingsDiagnostic {
    pub range: Range<usize>,
    pub message: String,
}

#[derive(Clone, Copy)]
pub enum SettingsPathRef<'a> {
    Builtin(&'a str),
    Path(&'a Path),
}

impl Display for SettingsPathRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Builtin(path) => write!(f, "built-in file {}", path),
            Self::Path(path) => write!(f, "{:?}", path),
        }
    }
}

impl SettingsPathRef<'_> {
    pub fn diagnostics_to_string(
        self,
        limit: usize,
        content: &str,
        mut diagnostics: Vec<SettingsDiagnostic>,
    ) -> Option<String> {
        if diagnostics.is_empty() || limit == 0 {
            return None;
        }

        let mut rope = Rope::new();
        rope.push(content);

        let diagnostic_to_string = |diagnostic: &SettingsDiagnostic| {
            dbg!(&diagnostic.range);
            let start = rope.offset_to_point(diagnostic.range.start);
            let end = rope.offset_to_point(diagnostic.range.end);
            dbg!(&start);
            dbg!(&end);
            if start.row == end.row {
                format!(
                    "at {}:{}-{}: {}",
                    start.row + 1,
                    start.column + 1,
                    end.column + 1,
                    diagnostic.message
                )
            } else {
                format!(
                    "at {}:{}-{}:{}: {}",
                    start.row + 1,
                    start.column + 1,
                    end.row + 1,
                    end.column + 1,
                    diagnostic.message
                )
            }
        };

        if diagnostics.len() == 1 {
            Some(format!(
                "In {} {}",
                self,
                diagnostic_to_string(&diagnostics[0])
            ))
        } else {
            let original_length = diagnostics.len();
            diagnostics.sort_by_key(|diagnostic| diagnostic.range.start);
            diagnostics.truncate(limit);

            let mut lines = Vec::with_capacity(diagnostics.len() + 1);
            lines.push(format!("{} errors in {}:", diagnostics.len(), self));
            for diagnostic in diagnostics.iter() {
                lines.push("".to_owned());
                lines.push(diagnostic_to_string(&diagnostic))
            }

            if diagnostics.len() < original_length {
                let omitted_count = diagnostics.len() - original_length;
                if omitted_count > 1 {
                    lines.push(format!("... {} more errors omitted", omitted_count));
                } else {
                    lines.push("... 1 more error omitted".to_owned());
                }
            }

            Some(lines.join("\n"))
        }
    }
}
