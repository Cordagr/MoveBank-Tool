use anyhow::{bail, Context, Result};
use chrono::{Duration, NaiveDateTime};
use clap::Parser;
use mb_client::{Auth, Client, EventQuery};
use mb_tageffect::{
    compare_periods, fetch_study_analysis, summarize_effects, MatchReport, PeriodComparison,
};
use std::io::{self, IsTerminal, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(about = "Analyze daily movement around deployment")]
struct Args {
    #[arg(long, help = "Movebank study ID")]
    study_id: i64,
    #[arg(long, value_parser = parse_datetime, help = "Earliest event timestamp (YYYY-MM-DDTHH:MM:SS)")]
    start: NaiveDateTime,
    #[arg(long, value_parser = parse_datetime, help = "Latest event timestamp (YYYY-MM-DDTHH:MM:SS)")]
    end: NaiveDateTime,
    #[arg(
        long,
        help = "Maximum time between fixes to count a movement segment, in minutes"
    )]
    max_gap_minutes: u64,
    #[arg(long, help = "Movebank sensor type ID to include")]
    sensor_type_id: Option<i64>,
    #[arg(
        long,
        default_value = "tageffect-daily.csv",
        help = "Daily movement CSV output"
    )]
    out: PathBuf,
    #[arg(long, help = "Early period first day after deployment, inclusive")]
    early_start_day: Option<u32>,
    #[arg(long, help = "Early period end day, exclusive")]
    early_end_day: Option<u32>,
    #[arg(long, help = "Baseline period first day after deployment, inclusive")]
    baseline_start_day: Option<u32>,
    #[arg(long, help = "Baseline period end day, exclusive")]
    baseline_end_day: Option<u32>,
    #[arg(
        long,
        default_value_t = 1,
        help = "Minimum observed days required in each period"
    )]
    min_observed_days: usize,
    #[arg(
        long,
        default_value = "tageffect-effects.csv",
        help = "Period comparison CSV output"
    )]
    effects_out: PathBuf,
    #[arg(long, default_value_t = 2_000, help = "Bootstrap resamples")]
    bootstrap_iterations: usize,
    #[arg(long, default_value_t = 0.95, help = "Bootstrap confidence level")]
    confidence_level: f64,
    #[arg(long, default_value_t = 42, help = "Bootstrap random seed")]
    seed: u64,
    #[arg(long, help = "After reading license terms, prompt to accept them")]
    accept_license: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.study_id <= 0 {
        bail!("study ID must be positive");
    }
    if args.start >= args.end {
        bail!("--start must be earlier than --end");
    }
    if args.max_gap_minutes == 0 {
        bail!("--max-gap-minutes must be greater than zero");
    }
    if args.min_observed_days == 0 {
        bail!("--min-observed-days must be greater than zero");
    }
    let max_gap_minutes =
        i64::try_from(args.max_gap_minutes).context("--max-gap-minutes is too large")?;
    let max_gap =
        Duration::try_minutes(max_gap_minutes).context("--max-gap-minutes is too large")?;
    let comparison_ranges = args.comparison_ranges()?;
    if let Some((early_days, baseline_days)) = &comparison_ranges {
        if early_days.start >= early_days.end
            || baseline_days.start >= baseline_days.end
            || (early_days.start < baseline_days.end && baseline_days.start < early_days.end)
        {
            bail!("comparison day ranges must be nonempty and must not overlap");
        }
        if args.bootstrap_iterations == 0
            || !args.confidence_level.is_finite()
            || args.confidence_level <= 0.0
            || args.confidence_level >= 1.0
        {
            bail!("bootstrap iterations must be positive and confidence level must be between 0 and 1");
        }
    }
    let client = movebank_client()?;
    let accept_license = args.accept_license;
    let mut approve_license = |study_id, terms: &[u8]| -> io::Result<bool> {
        eprintln!(
            "Movebank license terms for study {study_id}:\n{}",
            String::from_utf8_lossy(terms)
        );
        if !accept_license {
            eprintln!("Review the terms, then rerun with --accept-license to confirm.");
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

    let analysis = fetch_study_analysis(
        &client,
        &EventQuery {
            study_id: args.study_id,
            individuals: None,
            sensor_type_id: args.sensor_type_id,
            start: Some(args.start),
            end: Some(args.end),
        },
        max_gap,
        &mut approve_license,
    )
    .await
    .context("could not load and analyze study records")?;

    write_daily_csv(&args.out, &analysis.daily_movements)
        .with_context(|| format!("could not write {}", args.out.display()))?;
    println!(
        "Wrote {} daily rows to {}",
        analysis.daily_movements.len(),
        args.out.display()
    );
    eprintln!(
        "Matching: {} unique, {} unmatched, {} ambiguous fixes; {} deployments had no candidate fixes.",
        analysis.match_report.matches.len(),
        analysis.match_report.unmatched_fixes.len(),
        analysis.match_report.ambiguous_fixes.len(),
        analysis.match_report.unmatched_deployments.len()
    );
    if analysis.match_report.matches.is_empty() {
        report_match_diagnostics(&analysis.match_report);
    }

    if let Some((early_days, baseline_days)) = comparison_ranges {
        let comparisons = compare_periods(
            &analysis.daily_movements,
            early_days,
            baseline_days,
            args.min_observed_days,
        )
        .context("could not compare selected periods")?;
        write_comparisons_csv(&args.effects_out, &comparisons)
            .with_context(|| format!("could not write {}", args.effects_out.display()))?;
        println!(
            "Wrote {} period comparisons to {}",
            comparisons.len(),
            args.effects_out.display()
        );

        if comparisons.is_empty() {
            eprintln!("No deployments had enough observed days in both periods for a comparison.");
        } else {
            let summary = summarize_effects(
                &comparisons,
                args.bootstrap_iterations,
                args.confidence_level,
                args.seed,
            )
            .context("could not summarize individual effects")?;
            println!(
                "Mean early-minus-baseline distance: {:.3} km/day across {} individuals ({}% bootstrap CI: {:.3} to {:.3}).",
                summary.mean_difference_km,
                summary.individuals,
                (args.confidence_level * 100.0).round() as u32,
                summary.confidence_interval.0,
                summary.confidence_interval.1
            );
            println!("This is a post-deployment signal, not proof that tags caused the change.");
        }
    }

    Ok(())
}

impl Args {
    fn comparison_ranges(&self) -> Result<Option<(Range<u32>, Range<u32>)>> {
        match (
            self.early_start_day,
            self.early_end_day,
            self.baseline_start_day,
            self.baseline_end_day,
        ) {
            (None, None, None, None) => Ok(None),
            (Some(early_start), Some(early_end), Some(baseline_start), Some(baseline_end)) => {
                Ok(Some((early_start..early_end, baseline_start..baseline_end)))
            }
            _ => bail!("provide all four early and baseline day bounds to compare periods"),
        }
    }
}

fn parse_datetime(value: &str) -> std::result::Result<NaiveDateTime, String> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .map_err(|_| "timestamp must use YYYY-MM-DDTHH:MM:SS format".to_owned())
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
        (true, false) => bail!("MOVEBANK_PASSWORD is required when MOVEBANK_USER is set"),
        (false, true) => bail!("MOVEBANK_USER is required when MOVEBANK_PASSWORD is set"),
    }
}

fn write_daily_csv(path: &Path, rows: &[mb_tageffect::DailyMovement]) -> Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record([
        "deployment_id",
        "individual_id",
        "day_since_deployment",
        "distance_km",
        "usable_segments",
    ])?;
    for row in rows {
        writer.write_record([
            row.deployment_id.to_string(),
            row.individual_id.clone(),
            row.day_since_deployment.to_string(),
            row.distance_km
                .map_or_else(String::new, |value| value.to_string()),
            row.usable_segments.to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn write_comparisons_csv(path: &Path, rows: &[PeriodComparison]) -> Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record([
        "deployment_id",
        "individual_id",
        "early_mean_km_per_day",
        "baseline_mean_km_per_day",
        "difference_km_per_day",
        "early_days_observed",
        "baseline_days_observed",
    ])?;
    for row in rows {
        writer.write_record([
            row.deployment_id.to_string(),
            row.individual_id.clone(),
            row.early_mean_km.to_string(),
            row.baseline_mean_km.to_string(),
            row.difference_km.to_string(),
            row.early_days_observed.to_string(),
            row.baseline_days_observed.to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn report_match_diagnostics(report: &MatchReport) {
    if !report.unmatched_fixes.is_empty() {
        let earliest_event = report
            .unmatched_fixes
            .iter()
            .map(|fix| fix.timestamp)
            .min()
            .expect("nonempty unmatched fixes");
        let latest_event = report
            .unmatched_fixes
            .iter()
            .map(|fix| fix.timestamp)
            .max()
            .expect("nonempty unmatched fixes");
        eprintln!("Unmatched event timestamps span {earliest_event} through {latest_event}.");
        eprintln!("Sample event deployment IDs, individual IDs, and timestamps:");
        for fix in report.unmatched_fixes.iter().take(5) {
            eprintln!(
                "  deployment {:?}, individual {} at {}",
                fix.deployment_id, fix.individual_id, fix.timestamp
            );
        }
    } else if !report.ambiguous_fixes.is_empty() {
        eprintln!(
            "All {} returned event fixes matched multiple deployments and were excluded.",
            report.ambiguous_fixes.len()
        );
    } else {
        eprintln!("No event fixes were returned for this query. Check the study ID and requested date range.");
    }

    if report.unmatched_deployments.is_empty() {
        return;
    }

    let start_times: Vec<_> = report
        .unmatched_deployments
        .iter()
        .filter_map(|deployment| deployment.start_timestamp)
        .collect();
    let end_times: Vec<_> = report
        .unmatched_deployments
        .iter()
        .filter_map(|deployment| deployment.end_timestamp)
        .collect();

    if let (Some(earliest), Some(latest)) = (start_times.iter().min(), start_times.iter().max()) {
        eprintln!("Unmatched deployment start timestamps span {earliest} through {latest}.");
    } else {
        eprintln!("Unmatched deployments have no start timestamps.");
    }
    if let (Some(earliest), Some(latest)) = (end_times.iter().min(), end_times.iter().max()) {
        eprintln!("Unmatched deployment end timestamps span {earliest} through {latest}.");
    }
    eprintln!("Sample deployment IDs, individual IDs, and active dates:");
    for deployment in report.unmatched_deployments.iter().take(5) {
        eprintln!(
            "  deployment {}: individual {:?}, start {:?}, end {:?}",
            deployment.deployment_id,
            deployment.individual_id,
            deployment.start_timestamp,
            deployment.end_timestamp
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_iso_local_timestamp() {
        let timestamp = parse_datetime("2024-01-02T03:04:05").expect("timestamp should parse");
        assert_eq!(timestamp.to_string(), "2024-01-02 03:04:05");
    }

    #[test]
    fn comparison_windows_must_be_provided_together() {
        let args = Args::try_parse_from([
            "mbtagcheck",
            "--study-id",
            "1",
            "--start",
            "2024-01-01T00:00:00",
            "--end",
            "2024-02-01T00:00:00",
            "--max-gap-minutes",
            "60",
            "--early-start-day",
            "0",
        ])
        .expect("arguments should parse");

        assert!(args.comparison_ranges().is_err());
    }
}
