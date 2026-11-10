# Movebank Tools

A Rust workspace for browsing Movebank studies and exploring daily movement
around tag deployment. It includes a study browser and a command-line tag-effect
checker.

## Tools

- **`mbbrowse`** lists and filters studies, resolves taxon names using manual
  overrides and GBIF, and exports a ranked CSV.
- **`mbtagcheck`** fetches study events and deployments, matches fixes to
  deployments, and exports daily movement distances. It can also compare
  configured early and baseline periods using bootstrap confidence intervals.

## Setup

Install Rust 1.80 or later. Public studies may work without an account. For
account access, enter credentials in the current PowerShell session:

```powershell
$credential = Get-Credential
$env:MOVEBANK_USER = $credential.UserName
$env:MOVEBANK_PASSWORD = $credential.GetNetworkCredential().Password
```

Keep credentials out of source code and logs. They remain available to programs
started from that shell; close the shell when you are done.

Build and run the tests:

```powershell
cargo test --workspace
```

## Browse studies

Start with a small run to check that the client works:

```powershell
cargo run -p mbbrowse -- --limit-studies 10 --no-gbif --out preview.csv
```

Then apply filters for a wider search:

```powershell
cargo run -p mbbrowse -- `
  --class reptilia `
  --min-individuals 5 `
  --min-days 90 `
  --downloadable `
  --overrides data/taxon_overrides.csv `
  --out studies.csv
```

Use `--study-id ID` to process one study, `--limit-studies N` to limit the
number processed, or `--no-gbif` to use manual overrides without GBIF lookups.
The CSV is ranked by `neglect_score` (highest first). Its columns include study
ID and name, raw taxon and class, individual and location counts, duration,
downloadability, and score. Unknown taxa are reported in the terminal so they
can be reviewed or added to the override file.

## Check movement around deployment

Choose a study you are allowed to use and a date window that covers its event
data. Replace the example ID and dates below:

```powershell
cargo run -p mb-tageffect --bin mbtagcheck -- `
  --study-id 123456789 `
  --start "2024-01-01T00:00:00" `
  --end "2024-02-01T00:00:00" `
  --max-gap-minutes 60 `
  --out daily.csv `
  --accept-license
```

When license terms are presented, read them and type `ACCEPT` only if you agree.
The confirmation is interactive; `--accept-license` alone does not accept the
terms. Accepted terms are recorded locally for that study, account, and terms
version. Follow the study's license and citation requirements when using or
sharing results.

The daily CSV has one row per deployment and deployment-day:

| Column | Meaning |
|---|---|
| `deployment_id`, `individual_id` | Matched deployment and animal |
| `day_since_deployment` | Elapsed day since deployment |
| `distance_km` | Sum of usable distances between consecutive fixes |
| `usable_segments` | Number of fix-to-fix segments included |

Segments with invalid coordinates or gaps longer than `--max-gap-minutes` are
excluded. Check the terminal summary for matched, unmatched, or ambiguous fixes
and deployments with no candidate fixes. No rows can mean the selected dates
miss the deployment period or there were no usable segments.

### Compare early and baseline periods

Add all four day bounds to the command to create `tageffect-effects.csv` as
well. Day ranges are relative to deployment; starts are inclusive and ends are
exclusive. This example compares days 0-2 with days 7-13:

```powershell
cargo run -p mb-tageffect --bin mbtagcheck -- `
  --study-id 123456789 `
  --start "2024-01-01T00:00:00" `
  --end "2024-02-01T00:00:00" `
  --max-gap-minutes 60 `
  --early-start-day 0 `
  --early-end-day 3 `
  --baseline-start-day 7 `
  --baseline-end-day 14 `
  --min-observed-days 2 `
  --out daily.csv `
  --effects-out comparisons.csv `
  --accept-license
```

The comparison CSV reports early and baseline mean distance per day and their
difference for each deployment. The terminal also reports the mean
early-minus-baseline difference across individuals and a bootstrap confidence
interval. Positive values mean greater distance in the early period; this is
still a post-deployment signal, not proof of a tag effect. The default bootstrap
uses 2,000 iterations and seed 42; see `mbtagcheck --help` for options.

Pick day ranges that the study's data can support. Periods must not overlap,
and deployments need enough observed days in both periods. Timestamps are
handled as supplied by Movebank; the tool does not infer a timezone.

If a request returns `401`, check that both credential variables are set. If
there are no matches or daily rows, use the printed diagnostics to check the
study ID, date window, deployment dates, and fix coverage before changing the
gap threshold.

## Workspace

| Crate | Purpose |
|---|---|
| `mb-client` | Movebank API access, caching, and license handling |
| `mb-taxonomy` | Taxon normalization, overrides, and GBIF lookup |
| `mb-neglect` | Study table, filters, and ranking |
| `mbbrowse` | Study-browser command |
| `mb-tageffect` | Deployment matching and daily movement analysis |

Study metadata and generated cache files are kept locally; cache contents should
not be committed. Taxon overrides are maintained in
`data/taxon_overrides.csv`.
