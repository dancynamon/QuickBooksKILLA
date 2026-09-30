//! `ledger import shopify-payouts | shopify-orders | amazon-unified |
//! amazon-settlement-v2 | walmart-recon | etsy`, `ledger report
//! channel-sales | channel-recon`, `ledger sku-map`, `ledger quarantine`.
//! D26, `docs/CHANNEL.md`.
//!
//! A child module of the binary, so it shares `main.rs`'s hand-rolled
//! argument handling and usage-error shape.

use std::fs;
use std::path::PathBuf;

use chrono::NaiveDate;

use ledger::channel::report::{self, By};
use ledger::channel::stubs::{ChannelParser, EtsyParser, WalmartReconParser};
use ledger::channel::{amazon, db, shopify, Channel, ImportSummary};
use ledger::store::Ledger;

use super::{next_value, parse_date, runtime, Args, CommandError, UsageError};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
    Text,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelCommand {
    ShopifyPayouts {
        db: PathBuf,
        company: String,
        payouts: PathBuf,
        transactions: PathBuf,
        orders: Option<PathBuf>,
        lines: Option<PathBuf>,
        refunds: Option<PathBuf>,
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
        fallback_class: Option<String>,
    },
    ShopifyOrders {
        db: PathBuf,
        company: String,
        orders: PathBuf,
        lines: PathBuf,
        refunds: Option<PathBuf>,
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
    },
    AmazonUnified {
        db: PathBuf,
        company: String,
        file: PathBuf,
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
        fallback_class: Option<String>,
    },
    AmazonV2 {
        db: PathBuf,
        company: String,
        files: Vec<PathBuf>,
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
        fallback_class: Option<String>,
    },
    Walmart,
    Etsy,
    Sales {
        db: PathBuf,
        company: String,
        from: NaiveDate,
        to: NaiveDate,
        by: By,
        channel: Option<Channel>,
        format: Format,
    },
    Recon {
        db: PathBuf,
        company: String,
        from: NaiveDate,
        to: NaiveDate,
        format: Format,
    },
    SkuList {
        db: PathBuf,
        company: String,
        unmapped: bool,
    },
    SkuSet {
        db: PathBuf,
        company: String,
        channel: String,
        sku: String,
        family: String,
    },
    Quarantine {
        db: PathBuf,
        company: String,
    },
}

/// True when `ledger import <first>` names a channel importer rather than
/// the replica import's flags.
pub fn is_channel_import(first: Option<&String>) -> bool {
    first.is_some_and(|f| !f.starts_with('-'))
}

#[derive(Default)]
struct Flags {
    db: Option<PathBuf>,
    company: Option<String>,
    files: Vec<(String, PathBuf)>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    by: Option<String>,
    channel: Option<String>,
    format: Option<String>,
    fallback_class: Option<String>,
    unmapped: bool,
    sku: Option<String>,
    family: Option<String>,
}

fn read_flags(mut args: Args<'_>) -> Result<Flags, UsageError> {
    let mut f = Flags::default();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => f.db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => f.company = Some(next_value(&mut args, "--company")?),
            "--file" | "--payouts" | "--transactions" | "--orders" | "--lines" | "--refunds" => {
                let v = PathBuf::from(next_value(&mut args, flag)?);
                f.files.push((flag.trim_start_matches("--").to_string(), v));
            }
            "--from" => f.from = Some(parse_date(&next_value(&mut args, "--from")?, "--from")?),
            "--to" => f.to = Some(parse_date(&next_value(&mut args, "--to")?, "--to")?),
            "--by" => f.by = Some(next_value(&mut args, "--by")?),
            "--channel" => f.channel = Some(next_value(&mut args, "--channel")?),
            "--format" => f.format = Some(next_value(&mut args, "--format")?),
            "--fallback-class" => f.fallback_class = Some(next_value(&mut args, "--fallback-class")?),
            "--unmapped" => f.unmapped = true,
            "--sku" => f.sku = Some(next_value(&mut args, "--sku")?),
            "--family" => f.family = Some(next_value(&mut args, "--family")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(f)
}

impl Flags {
    fn db(&self) -> Result<PathBuf, UsageError> {
        self.db.clone().ok_or_else(|| UsageError::missing_flag("--db"))
    }
    fn company(&self) -> Result<String, UsageError> {
        self.company
            .clone()
            .ok_or_else(|| UsageError::missing_flag("--company"))
    }
    fn file(&self, name: &str) -> Option<PathBuf> {
        self.files.iter().find(|(n, _)| n == name).map(|(_, p)| p.clone())
    }
    fn need_file(&self, name: &str) -> Result<PathBuf, UsageError> {
        self.file(name)
            .ok_or_else(|| UsageError::missing_flag(&format!("--{name}")))
    }
    fn format(&self, default: Format) -> Result<Format, UsageError> {
        match self.format.as_deref() {
            None => Ok(default),
            Some("csv") => Ok(Format::Csv),
            Some("json") => Ok(Format::Json),
            Some("text") => Ok(Format::Text),
            Some(other) => Err(UsageError::invalid_value("--format", other)),
        }
    }
}

pub fn parse_import(mut args: Args<'_>) -> Result<ChannelCommand, UsageError> {
    let Some(sub) = args.next() else {
        return Err(UsageError::new("import requires a subcommand or --replica"));
    };
    let f = read_flags(args)?;
    match sub.as_str() {
        "shopify-payouts" => Ok(ChannelCommand::ShopifyPayouts {
            db: f.db()?,
            company: f.company()?,
            payouts: f.need_file("payouts")?,
            transactions: f.need_file("transactions")?,
            orders: f.file("orders"),
            lines: f.file("lines"),
            refunds: f.file("refunds"),
            from: f.from,
            to: f.to,
            fallback_class: f.fallback_class.clone(),
        }),
        "shopify-orders" => Ok(ChannelCommand::ShopifyOrders {
            db: f.db()?,
            company: f.company()?,
            orders: f.need_file("orders")?,
            lines: f.need_file("lines")?,
            refunds: f.file("refunds"),
            from: f.from,
            to: f.to,
        }),
        "amazon-unified" => Ok(ChannelCommand::AmazonUnified {
            db: f.db()?,
            company: f.company()?,
            file: f.need_file("file")?,
            from: f.from,
            to: f.to,
            fallback_class: f.fallback_class.clone(),
        }),
        "amazon-settlement-v2" => {
            let files: Vec<PathBuf> = f
                .files
                .iter()
                .filter(|(n, _)| n == "file")
                .map(|(_, p)| p.clone())
                .collect();
            if files.is_empty() {
                return Err(UsageError::missing_flag("--file"));
            }
            Ok(ChannelCommand::AmazonV2 {
                db: f.db()?,
                company: f.company()?,
                files,
                from: f.from,
                to: f.to,
                fallback_class: f.fallback_class.clone(),
            })
        }
        "walmart-recon" => Ok(ChannelCommand::Walmart),
        "etsy" => Ok(ChannelCommand::Etsy),
        other => Err(UsageError::unknown_subcommand(&format!("import {other}"))),
    }
}

pub fn parse_report(mut args: Args<'_>) -> Result<ChannelCommand, UsageError> {
    let Some(sub) = args.next() else {
        return Err(UsageError::new("report requires a subcommand (channel-sales, channel-recon)"));
    };
    let f = read_flags(args)?;
    let from = f.from.ok_or_else(|| UsageError::missing_flag("--from"))?;
    let to = f.to.ok_or_else(|| UsageError::missing_flag("--to"))?;
    match sub.as_str() {
        "channel-sales" => {
            let by = match f.by.as_deref() {
                None | Some("family") => By::Family,
                Some("sku") => By::Sku,
                Some(other) => return Err(UsageError::invalid_value("--by", other)),
            };
            let channel = match f.channel.as_deref() {
                None => None,
                Some(c) => Some(
                    Channel::parse(c).ok_or_else(|| UsageError::invalid_value("--channel", c))?,
                ),
            };
            Ok(ChannelCommand::Sales {
                db: f.db()?,
                company: f.company()?,
                from,
                to,
                by,
                channel,
                format: f.format(Format::Csv)?,
            })
        }
        "channel-recon" => Ok(ChannelCommand::Recon {
            db: f.db()?,
            company: f.company()?,
            from,
            to,
            format: f.format(Format::Csv)?,
        }),
        other => Err(UsageError::unknown_subcommand(&format!("report {other}"))),
    }
}

pub fn parse_sku_map(mut args: Args<'_>) -> Result<ChannelCommand, UsageError> {
    let Some(sub) = args.next() else {
        return Err(UsageError::new("sku-map requires a subcommand (list, unmapped, set)"));
    };
    let f = read_flags(args)?;
    match sub.as_str() {
        "list" | "unmapped" => Ok(ChannelCommand::SkuList {
            db: f.db()?,
            company: f.company()?,
            unmapped: sub == "unmapped" || f.unmapped,
        }),
        "set" => Ok(ChannelCommand::SkuSet {
            db: f.db()?,
            company: f.company()?,
            channel: f.channel.clone().ok_or_else(|| UsageError::missing_flag("--channel"))?,
            sku: f.sku.clone().ok_or_else(|| UsageError::missing_flag("--sku"))?,
            family: f.family.clone().ok_or_else(|| UsageError::missing_flag("--family"))?,
        }),
        other => Err(UsageError::unknown_subcommand(&format!("sku-map {other}"))),
    }
}

pub fn parse_quarantine(args: Args<'_>) -> Result<ChannelCommand, UsageError> {
    let f = read_flags(args)?;
    Ok(ChannelCommand::Quarantine {
        db: f.db()?,
        company: f.company()?,
    })
}

fn read(path: &std::path::Path) -> Result<String, CommandError> {
    fs::read_to_string(path).map_err(|e| runtime(format!("{}: {e}", path.display())))
}

fn print_summary(summary: &ImportSummary) {
    print!("{}", summary.render_text());
}

pub fn run(cmd: ChannelCommand) -> Result<std::process::ExitCode, CommandError> {
    use std::process::ExitCode;
    match cmd {
        ChannelCommand::ShopifyPayouts {
            db,
            company,
            payouts,
            transactions,
            orders,
            lines,
            refunds,
            from,
            to,
            fallback_class,
        } => {
            let ledger = Ledger::open(&db).map_err(runtime)?;
            let book = match (&orders, &lines) {
                (Some(o), Some(l)) => {
                    let refunds_text = refunds.as_deref().map(read).transpose()?;
                    Some(
                        shopify::OrderBook::parse(&read(o)?, &read(l)?, refunds_text.as_deref())
                            .map_err(runtime)?,
                    )
                }
                (None, None) => None,
                _ => return Err(runtime("--orders and --lines go together")),
            };
            let mut cfg = shopify::ShopifyConfig::default();
            if let Some(c) = fallback_class {
                cfg.fallback_class = c;
            }
            let summary = shopify::import_payouts(
                &ledger,
                &company,
                &read(&payouts)?,
                &read(&transactions)?,
                book.as_ref(),
                &cfg,
                from,
                to,
            )
            .map_err(runtime)?;
            print_summary(&summary);
            Ok(exit(&summary))
        }
        ChannelCommand::ShopifyOrders {
            db,
            company,
            orders,
            lines,
            refunds,
            from,
            to,
        } => {
            let ledger = Ledger::open(&db).map_err(runtime)?;
            let refunds_text = refunds.as_deref().map(read).transpose()?;
            let book = shopify::OrderBook::parse(&read(&orders)?, &read(&lines)?, refunds_text.as_deref())
                .map_err(runtime)?;
            let summary = shopify::import_orders(&ledger, &company, &book, from, to).map_err(runtime)?;
            print_summary(&summary);
            Ok(exit(&summary))
        }
        ChannelCommand::AmazonUnified {
            db,
            company,
            file,
            from,
            to,
            fallback_class,
        } => {
            let ledger = Ledger::open(&db).map_err(runtime)?;
            let mut cfg = amazon::AmazonConfig::default();
            if let Some(c) = fallback_class {
                cfg.fallback_class = c;
            }
            let summary = amazon::import_unified(&ledger, &company, &read(&file)?, &cfg, from, to)
                .map_err(runtime)?;
            print_summary(&summary);
            Ok(exit(&summary))
        }
        ChannelCommand::AmazonV2 {
            db,
            company,
            files,
            from,
            to,
            fallback_class,
        } => {
            let ledger = Ledger::open(&db).map_err(runtime)?;
            let mut cfg = amazon::AmazonConfig::default();
            if let Some(c) = fallback_class {
                cfg.fallback_class = c;
            }
            let mut worst = ExitCode::SUCCESS;
            for file in files {
                println!("{}", file.display());
                let summary = amazon::import_v2(&ledger, &company, &read(&file)?, &cfg, from, to)
                    .map_err(runtime)?;
                print_summary(&summary);
                if exit(&summary) != ExitCode::SUCCESS {
                    worst = ExitCode::from(1);
                }
            }
            Ok(worst)
        }
        ChannelCommand::Walmart => {
            WalmartReconParser.parse("").map_err(runtime)?;
            Ok(ExitCode::SUCCESS)
        }
        ChannelCommand::Etsy => {
            EtsyParser.parse("").map_err(runtime)?;
            Ok(ExitCode::SUCCESS)
        }
        ChannelCommand::Sales {
            db,
            company,
            from,
            to,
            by,
            channel,
            format,
        } => {
            let ledger = Ledger::open(&db).map_err(runtime)?;
            let rows = report::channel_sales(&ledger, &company, from, to, by, channel).map_err(runtime)?;
            match format {
                Format::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&report::sales_json(&rows, from, to, by))
                        .map_err(runtime)?
                ),
                _ => print!("{}", report::sales_csv(&rows, by)),
            }
            Ok(ExitCode::SUCCESS)
        }
        ChannelCommand::Recon {
            db,
            company,
            from,
            to,
            format,
        } => {
            let ledger = Ledger::open(&db).map_err(runtime)?;
            let rows = report::channel_recon(&ledger, &company, from, to).map_err(runtime)?;
            match format {
                Format::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&report::recon_json(&rows)).map_err(runtime)?
                ),
                _ => print!("{}", report::recon_csv(&rows)),
            }
            Ok(ExitCode::SUCCESS)
        }
        ChannelCommand::SkuList {
            db,
            company,
            unmapped,
        } => {
            let ledger = Ledger::open(&db).map_err(runtime)?;
            for r in db::list_sku_map(&ledger, &company, unmapped).map_err(runtime)? {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    r.channel,
                    r.sku,
                    r.family,
                    r.source,
                    r.title.unwrap_or_default()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        ChannelCommand::SkuSet {
            db,
            company,
            channel,
            sku,
            family,
        } => {
            let ledger = Ledger::open(&db).map_err(runtime)?;
            db::set_manual_sku(&ledger, &company, &channel, &sku, &family, chrono::Utc::now())
                .map_err(runtime)?;
            println!("{channel} {sku} -> {family} (manual)");
            Ok(ExitCode::SUCCESS)
        }
        ChannelCommand::Quarantine { db, company } => {
            let ledger = Ledger::open(&db).map_err(runtime)?;
            let rows = db::list_quarantine(&ledger, &company).map_err(runtime)?;
            for r in &rows {
                println!("{}\t{}\t{}\t{}", r.external_id, r.channel, r.reason, r.detail);
            }
            println!("{} quarantined", rows.len());
            Ok(if rows.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) })
        }
    }
}

/// Non-zero when something was quarantined or rejected: a clean import is
/// the only quiet one.
fn exit(summary: &ImportSummary) -> std::process::ExitCode {
    if summary.quarantined.is_empty() && summary.rejected.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
