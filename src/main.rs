use snomed_compiler::{compile, format, query};

#[cfg(feature = "serve")]
mod serve;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "snomed-compile",
    about = "SNOMED CT RF2 compiler and query tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile an RF2 release directory into a binary artifact.
    Compile {
        /// Path to the RF2 release directory (Snapshot or Full).
        #[arg(long)]
        rf2_dir: PathBuf,

        /// Output file path for the compiled artifact.
        #[arg(long)]
        output: PathBuf,

        /// Release date as YYYYMMDD (e.g. 20250501). Stored in artifact header.
        #[arg(long, default_value = "0")]
        date: u32,

        /// Edition: international | uk  (default: international)
        #[arg(long, default_value = "international")]
        edition: String,
    },

    /// Query a compiled artifact.
    Query {
        /// Path to the compiled artifact.
        #[arg(long)]
        db: PathBuf,

        #[command(subcommand)]
        op: QueryOp,
    },

    /// Launch axum webserver that can be used to serve results
    #[cfg(feature = "serve")]
    Serve {
        /// Path to compiled artifact
        #[arg(long)]
        db: PathBuf,
    },
}

#[derive(Subcommand)]
enum QueryOp {
    /// Print all descendants of SCTID (one per line).
    Descendants { sctid: u64 },

    /// Print all ancestors of SCTID (one per line).
    Ancestors { sctid: u64 },

    /// Print direct parents of SCTID (one per line).
    Parents { sctid: u64 },

    /// Print direct children of SCTID (one per line).
    Children { sctid: u64 },

    /// Test whether CHILD is a (strict) descendant of ANCESTOR.
    #[command(name = "is-a")]
    IsA { child: u64, ancestor: u64 },

    /// Print basic metadata for a concept.
    Concept { sctid: u64 },
}

// ── entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Compile {
            rf2_dir,
            output,
            date,
            edition,
        } => {
            let edition_code = match edition.as_str() {
                "international" => format::EDITION_INTERNATIONAL,
                "uk" => format::EDITION_UK,
                _ => format::EDITION_UNKNOWN,
            };
            compile::compile(compile::CompileOptions {
                rf2_dir,
                output,
                release_date: date,
                edition: edition_code,
            })?;
        }

        #[cfg(feature = "serve")]
        Command::Serve { db } => {
            let rt = tokio::runtime::Runtime::new()?;

            rt.block_on(serve::run_server(db));
        }

        Command::Query { db, op } => {
            let db = query::SnomedDb::open(&db)?;
            match op {
                QueryOp::Descendants { sctid } => {
                    for s in db.descendants(sctid) {
                        println!("{s}");
                    }
                }
                QueryOp::Ancestors { sctid } => {
                    for s in db.ancestors(sctid) {
                        println!("{s}");
                    }
                }
                QueryOp::Parents { sctid } => {
                    for s in db.parents(sctid) {
                        println!("{s}");
                    }
                }
                QueryOp::Children { sctid } => {
                    for s in db.children(sctid) {
                        println!("{s}");
                    }
                }
                QueryOp::IsA { child, ancestor } => {
                    println!("{}", db.is_descendant_of(child, ancestor));
                }
                QueryOp::Concept { sctid } => match db.concept(sctid) {
                    Some(c) => println!(
                        "sctid:         {}\nactive:        {}\nfully_defined: {}",
                        c.sctid, c.active, c.fully_defined
                    ),
                    None => {
                        eprintln!("Concept {} not found in artifact", sctid);
                        std::process::exit(1);
                    }
                },
            }
        }
    }

    Ok(())
}
