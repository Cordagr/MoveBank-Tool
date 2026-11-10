use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use mb_client::{Auth, Client};
use mb_neglect::{Filter, NeglectError, StudyRow};
use mb_taxonomy::{GbifClient, Overrides, TaxonClass};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(about = "Browse Movebank studies and export a ranked CSV")]
struct Args {
    #[arg(long, value_enum, help = "Keep studies in this taxonomic class")]
    class: Option<ClassArg>,
    #[arg(long, help = "Keep studies with at least this many individuals")]
    min_individuals: Option<u32>,
    #[arg(
        long,
        value_parser = parse_days,
        help = "Keep studies with at least this many days of data"
    )]
    min_days: Option<f64>,
    #[arg(long, help = "Keep only studies available for download")]
    downloadable: bool,
    #[arg(long, default_value = "studies.csv", help = "CSV file to write")]
    out: PathBuf,
    #[arg(long, help = "CSV file containing manual taxon overrides")]
    overrides: Option<PathBuf>,
    #[arg(long, help = "Use manual overrides only; skip GBIF lookups")]
    no_gbif: bool,
    #[arg(long, help = "Process only the first N studies")]
    limit_studies: Option<usize>,
    #[arg(long, help = "Process only this Movebank study ID")]
    study_id: Option<i64>,
    #[arg(
        long,
        help = "After reviewing license terms, prompt before accepting them"
    )]
    accept_license: bool,
}

#[derive(Clone, Debug, ValueEnum)]
enum ClassArg {
    Aves,
    Mammalia,
    Reptilia,
    Amphibia,
    Fish,
    Invertebrate,
    Other,
    Unknown,
}

impl From<ClassArg> for TaxonClass {
    fn from(class: ClassArg) -> Self {
        match class {
            ClassArg::Aves => Self::Aves,
            ClassArg::Mammalia => Self::Mammalia,
            ClassArg::Reptilia => Self::Reptilia,
            ClassArg::Amphibia => Self::Amphibia,
            ClassArg::Fish => Self::Fish,
            ClassArg::Invertebrate => Self::Invertebrate,
            ClassArg::Other => Self::Other,
            ClassArg::Unknown => Self::Unknown,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let overrides = match args.overrides {
        Some(path) => Overrides::from_path(&path)
            .with_context(|| format!("could not load overrides from {}", path.display()))?,
        None => Overrides::new(),
    };
    let gbif = if args.no_gbif {
        None
    } else {
        Some(GbifClient::new().context("could not create GBIF client")?)
    };
    let client = movebank_client()?;
    let accept_license = args.accept_license;
    let mut approve_license = |study_id, terms: &[u8]| -> io::Result<bool> {
        eprintln!(
            "Movebank license terms for study {study_id}:\n{}",
            String::from_utf8_lossy(terms)
        );
        if !accept_license {
            return Ok(false);
        }
        if !io::stdin().is_terminal() {
            eprintln!("License acceptance requires an interactive terminal.");
            return Ok(false);
        }

        eprint!("Type ACCEPT if you have read and agree to these terms: ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        Ok(answer.trim() == "ACCEPT")
    };
    let mut rows = match StudyRow::build_study_table_with_license_approval(
        &client,
        &overrides,
        gbif.as_ref(),
        args.study_id,
        args.limit_studies,
        &mut approve_license,
    )
    .await
    {
        Ok(rows) => rows,
        Err(NeglectError::LicenseRequired { study_id, .. }) if !accept_license => {
            anyhow::bail!("review study {study_id} terms above, then rerun with --accept-license")
        }
        Err(NeglectError::LicenseRequired { study_id, .. }) => {
            anyhow::bail!("license terms for study {study_id} were not accepted")
        }
        Err(error) => return Err(error).context("could not build the study table"),
    };
    StudyRow::neglect_score(&mut rows, None).context("could not score the studies")?;

    report_classification(&rows);

    let filter = Filter {
        class: args.class.map(Into::into),
        min_individuals: args.min_individuals,
        min_days: args.min_days,
        downloadable: args.downloadable,
    };
    let mut rows = StudyRow::filter_studies(&rows, &filter);
    rows.sort_by(|left, right| {
        right
            .neglect_score
            .total_cmp(&left.neglect_score)
            .then_with(|| left.name.cmp(&right.name))
    });

    write_csv(&args.out, &rows)
        .with_context(|| format!("could not write {}", args.out.display()))?;
    println!("Wrote {} studies to {}", rows.len(), args.out.display());
    Ok(())
}

fn movebank_client() -> Result<Client> {
    let has_user = std::env::var_os("MOVEBANK_USER").is_some();
    let has_password = std::env::var_os("MOVEBANK_PASSWORD").is_some();

    match (has_user, has_password) {
        (true, true) => {
            Client::from_env().context("could not create authenticated Movebank client")
        }
        (false, false) => {
            Client::new(Auth::Anonymous).context("could not create anonymous Movebank client")
        }
        (true, false) => anyhow::bail!("MOVEBANK_PASSWORD is required when MOVEBANK_USER is set"),
        (false, true) => anyhow::bail!("MOVEBANK_USER is required when MOVEBANK_PASSWORD is set"),
    }
}

fn parse_days(value: &str) -> std::result::Result<f64, String> {
    let days = value
        .parse::<f64>()
        .map_err(|_| "days must be a number".to_owned())?;
    if days.is_finite() && days >= 0.0 {
        Ok(days)
    } else {
        Err("days must be a finite, nonnegative number".to_owned())
    }
}

fn report_classification(rows: &[StudyRow]) {
    let classified = rows
        .iter()
        .filter(|row| row.class != TaxonClass::Unknown)
        .count();
    let percentage = if rows.is_empty() {
        0.0
    } else {
        classified as f64 / rows.len() as f64 * 100.0
    };
    eprintln!(
        "Classified {classified} of {} studies ({percentage:.1}%).",
        rows.len()
    );

    for row in rows.iter().filter(|row| row.class == TaxonClass::Unknown) {
        let raw_taxon = if row.taxon_raw.is_empty() {
            "(no taxon reported)"
        } else {
            &row.taxon_raw
        };
        eprintln!(
            "Unresolved taxon for study {} ({}): {}",
            row.id, row.name, raw_taxon
        );
    }
}

fn write_csv(path: &Path, rows: &[StudyRow]) -> Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record([
        "id",
        "name",
        "taxon_raw",
        "class",
        "n_individuals",
        "n_locations",
        "duration_days",
        "downloadable",
        "neglect_score",
    ])?;

    for row in rows {
        writer.write_record([
            row.id.to_string(),
            row.name.clone(),
            row.taxon_raw.clone(),
            class_name(&row.class).to_owned(),
            optional_string(row.n_individuals),
            optional_string(row.n_locations),
            optional_string(row.duration_days),
            row.downloadable.to_string(),
            row.neglect_score.to_string(),
        ])?;
    }

    writer.flush()?;
    Ok(())
}

fn optional_string<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

fn class_name(class: &TaxonClass) -> &'static str {
    match class {
        TaxonClass::Aves => "aves",
        TaxonClass::Mammalia => "mammalia",
        TaxonClass::Reptilia => "reptilia",
        TaxonClass::Amphibia => "amphibia",
        TaxonClass::Fish => "fish",
        TaxonClass::Invertebrate => "invertebrate",
        TaxonClass::Other => "other",
        TaxonClass::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_days, Args};
    use clap::Parser;

    #[test]
    fn min_days_must_be_finite_and_nonnegative() {
        assert_eq!(parse_days("90").expect("valid days"), 90.0);
        assert!(parse_days("-1").is_err());
        assert!(parse_days("NaN").is_err());
    }

    #[test]
    fn parses_study_id() {
        let args = Args::try_parse_from(["mbbrowse", "--study-id", "2034278428"])
            .expect("study ID should parse");
        assert_eq!(args.study_id, Some(2_034_278_428));
    }
}
