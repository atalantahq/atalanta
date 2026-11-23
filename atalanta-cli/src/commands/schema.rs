//! `atalanta schema`: the JSON Schema for suite YAML, for editors.

use clap::Args;

#[derive(Args)]
pub struct SchemaArgs {
    /// Write to this file instead of stdout
    #[arg(short, long)]
    output: Option<String>,

    /// Print the DSL reference (docs/DSL-REFERENCE.md) as Markdown instead
    #[arg(long)]
    markdown: bool,
}

pub fn execute(args: SchemaArgs) -> anyhow::Result<i32> {
    if args.markdown {
        let md = atalanta_core::reference::dsl_reference();
        match args.output {
            Some(path) => std::fs::write(&path, md)?,
            None => print!("{md}"),
        }
        return Ok(0);
    }
    let json = serde_json::to_string_pretty(&atalanta_core::schema::suite_schema())?;
    match args.output {
        Some(path) => {
            std::fs::write(&path, json + "\n")?;
            eprintln!(
                "=> Wrote {path}. Point your editor at it with this first line in a suite:\n   # yaml-language-server: $schema={path}"
            );
        }
        None => println!("{json}"),
    }
    Ok(0)
}
