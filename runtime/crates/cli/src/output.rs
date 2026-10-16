use crate::ui;
use anyhow::Context;
use clap::ValueEnum;
use serde_json::Value;

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum QueryOutputFormat {
    Text,
    Table,
    Json,
}

impl From<rivo_session::QueryFormat> for QueryOutputFormat {
    fn from(value: rivo_session::QueryFormat) -> Self {
        match value {
            rivo_session::QueryFormat::Text => Self::Text,
            rivo_session::QueryFormat::Table => Self::Table,
            rivo_session::QueryFormat::Json => Self::Json,
        }
    }
}

impl From<QueryOutputFormat> for rivo_session::QueryFormat {
    fn from(value: QueryOutputFormat) -> Self {
        match value {
            QueryOutputFormat::Text => Self::Text,
            QueryOutputFormat::Table => Self::Table,
            QueryOutputFormat::Json => Self::Json,
        }
    }
}

pub(crate) fn resolve_query_format(
    explicit: Option<QueryOutputFormat>,
    configured: Option<rivo_session::QueryFormat>,
) -> QueryOutputFormat {
    explicit
        .or_else(|| configured.map(Into::into))
        .unwrap_or(QueryOutputFormat::Text)
}

pub(crate) fn load_query_format() -> anyhow::Result<Option<rivo_session::QueryFormat>> {
    let path = std::env::var_os("RIVO_CONFIG")
        .map(std::path::PathBuf::from)
        .or_else(rivo_session::config_file_path)
        .context(ui::explicit_text(
            "cli-output-config-path-unresolved",
            "Unable to determine the configuration file path",
        ))?;
    Ok(rivo_session::read_file_config(&path)?.defaults.query_format)
}

pub(crate) struct QueryOutput {
    pub(crate) text: String,
    pub(crate) table: Table,
    pub(crate) json: Value,
}

impl QueryOutput {
    pub(crate) fn new(text: String, table: Table, json: Value) -> Self {
        Self { text, table, json }
    }
}

pub(crate) struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub(crate) fn new(headers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            headers: headers.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
        }
    }

    pub(crate) fn push_row(&mut self, row: impl IntoIterator<Item = impl Into<String>>) {
        self.rows.push(row.into_iter().map(Into::into).collect());
    }

    fn render(&self) -> anyhow::Result<String> {
        if self.headers.is_empty() {
            return Ok(String::new());
        }
        if self.rows.iter().any(|row| row.len() != self.headers.len()) {
            anyhow::bail!(
                "{}",
                ui::explicit_text(
                    "cli-output-table-column-mismatch",
                    "Table row width does not match the header",
                )
            );
        }

        let widths = (0..self.headers.len())
            .map(|index| {
                self.rows
                    .iter()
                    .fold(display_width(&self.headers[index]), |width, row| {
                        width.max(display_width(&row[index]))
                    })
            })
            .collect::<Vec<_>>();
        let border = |left: char, join: char, right: char| {
            format!(
                "{left}{}{right}",
                widths
                    .iter()
                    .map(|width| "-".repeat(width + 2))
                    .collect::<Vec<_>>()
                    .join(&join.to_string())
            )
        };
        let render_row = |row: &[String]| {
            let cells = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| {
                    format!(" {}{} ", cell, " ".repeat(width - display_width(cell)))
                })
                .collect::<Vec<_>>()
                .join("|");
            format!("|{cells}|")
        };

        let mut result = vec![
            border('+', '+', '+'),
            render_row(&self.headers),
            border('+', '+', '+'),
        ];
        result.extend(self.rows.iter().map(|row| render_row(row)));
        result.push(border('+', '+', '+'));
        Ok(result.join("\n"))
    }
}

pub(crate) fn print_query(format: QueryOutputFormat, output: QueryOutput) -> anyhow::Result<()> {
    match format {
        QueryOutputFormat::Text => print!("{}", output.text),
        QueryOutputFormat::Table => println!("{}", output.table.render()?),
        QueryOutputFormat::Json => println!("{}", serde_json::to_string_pretty(&output.json)?),
    }
    Ok(())
}

pub(crate) fn parse_query_format(
    args: &mut Vec<String>,
) -> anyhow::Result<Option<QueryOutputFormat>> {
    let positions = args
        .iter()
        .enumerate()
        .filter_map(|(index, value)| (value == "--format").then_some(index))
        .collect::<Vec<_>>();
    if positions.len() > 1 {
        anyhow::bail!(
            "{}",
            ui::explicit_text("cli-output-format-duplicate", "--format cannot be repeated",)
        );
    }
    let Some(index) = positions.first().copied() else {
        return Ok(None);
    };
    let value = QueryOutputFormat::from_str(
        args.get(index + 1).context(ui::explicit_text(
            "cli-output-format-missing",
            "--format requires a value",
        ))?,
        false,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    args.drain(index..=index + 1);
    Ok(Some(value))
}

fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|character| if character.is_ascii() { 1 } else { 2 })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_ascii_table_with_headers_and_cjk_cells() {
        let mut table = Table::new(["ID", "Name"]);
        table.push_row(["openai", "OpenAI"]);
        table.push_row(["rivo", "中文"]);

        assert_eq!(
            table.render().unwrap(),
            "+--------+--------+\n| ID     | Name   |\n+--------+--------+\n| openai | OpenAI |\n| rivo   | 中文   |\n+--------+--------+"
        );
    }

    #[test]
    fn rejects_rows_with_wrong_column_count() {
        let mut table = Table::new(["ID", "Name"]);
        table.push_row(["only-one-cell"]);

        assert!(table.render().is_err());
    }

    #[test]
    fn parses_and_removes_query_format() {
        let mut args = vec!["list".into(), "--format".into(), "json".into()];

        assert_eq!(
            parse_query_format(&mut args).unwrap(),
            Some(QueryOutputFormat::Json)
        );
        assert_eq!(args, vec!["list"]);
    }

    #[test]
    fn query_format_priority_preserves_explicit_text() {
        let formats = [
            None,
            Some(QueryOutputFormat::Text),
            Some(QueryOutputFormat::Table),
            Some(QueryOutputFormat::Json),
        ];
        for explicit in formats {
            for configured in formats {
                assert_eq!(
                    resolve_query_format(explicit, configured.map(Into::into)),
                    explicit.or(configured).unwrap_or(QueryOutputFormat::Text)
                );
            }
        }
        assert_eq!(parse_query_format(&mut vec!["list".into()]).unwrap(), None);
        assert_eq!(
            parse_query_format(&mut vec!["--format".into(), "text".into()]).unwrap(),
            Some(QueryOutputFormat::Text)
        );
        for values in [
            vec!["--format"],
            vec!["--format", "jsonl"],
            vec!["--format", "json", "--format", "text"],
        ] {
            let mut args = values.into_iter().map(str::to_owned).collect();
            assert!(parse_query_format(&mut args).is_err());
        }
    }
}
