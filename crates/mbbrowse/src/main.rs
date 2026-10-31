use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use mb_client::{Auth, Client};
use mb_neglect::{Filter, StudyRow};
use mb_taxonomy::{GbifClient, Overrides, TaxonClass};
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
    let client = Client::new(Auth::Anonymous).context("could not create Movebank client")?;
    let mut rows = StudyRow::build_study_table(&client, &overrides, gbif.as_ref())
        .await
        .context("could not build the study table")?;
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
    use super::parse_days;

    #[test]
    fn min_days_must_be_finite_and_nonnegative() {
        assert_eq!(parse_days("90").expect("valid days"), 90.0);
        assert!(parse_days("-1").is_err());
        assert!(parse_days("NaN").is_err());
    }
}
