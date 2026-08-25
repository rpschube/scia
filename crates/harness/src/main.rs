//! The `scia-harness` command-line entry point: the corpus, run, A/B, verdict
//! and freeze subcommands. Argument parsing and I/O live here; the scoring,
//! replay and corpus logic live in the library so they are unit-tested directly.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use scia_harness::ab;
use scia_harness::clip;
use scia_harness::corpus;
use scia_harness::freeze::{DEFAULT_FLOOR, DEFAULT_MARGIN, Envelope};
use scia_harness::metrics::{MetricParams, Metrics};
use scia_harness::records::to_line;
use scia_harness::replay::{RunRequest, load_preset_labeled, run as replay_run};
use scia_harness::synth::{SYNTH_SPECS, synth_spec};
use scia_harness::verdict;

/// Scene-quality harness: replay golden clips through scenes, score the display
/// lists, and run the A/B / preference-log / envelope plumbing.
#[derive(Parser, Debug)]
#[command(name = "scia-harness", version, about)]
struct Cli {
    /// The quality directory holding the corpus, preference log and envelopes.
    #[arg(long, global = true, default_value = "quality")]
    quality_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage the golden-clip corpus.
    #[command(subcommand)]
    Corpus(CorpusCmd),
    /// Replay a clip through a scene and score the result.
    Run(RunArgs),
    /// Run two presets and compare their metrics side by side.
    Ab(AbArgs),
    /// Append a preference verdict to the log.
    Verdict(VerdictArgs),
    /// Freeze a metric envelope for a scene from an approved run.
    Freeze(FreezeArgs),
}

#[derive(Subcommand, Debug)]
enum CorpusCmd {
    /// Generate the deterministic synthetic clip(s) and refresh the manifest.
    Synth {
        /// Only (re)generate this clip id; default: every synthetic clip.
        #[arg(long)]
        id: Option<String>,
    },
    /// Verify every manifest entry's content hash (regenerating generated clips).
    Verify,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Clip id (from the manifest) or a path to a feature-stream file.
    #[arg(long)]
    clip: String,
    /// Scene id to drive.
    #[arg(long)]
    scene: String,
    /// Optional preset TOML file.
    #[arg(long)]
    preset: Option<String>,
    /// Parameter overrides, `key=value`, repeatable.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    sets: Vec<String>,
    /// Output directory for `run.jsonl` and `metrics.json`.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct AbArgs {
    /// Clip id (from the manifest) or a path to a feature-stream file.
    #[arg(long)]
    clip: String,
    /// Scene id to drive.
    #[arg(long)]
    scene: String,
    /// Candidate A preset TOML file.
    #[arg(long = "preset-a")]
    preset_a: String,
    /// Candidate B preset TOML file.
    #[arg(long = "preset-b")]
    preset_b: String,
}

#[derive(Args, Debug)]
struct VerdictArgs {
    /// Scene id the verdict is about.
    #[arg(long)]
    scene: String,
    /// Clip id the verdict is about.
    #[arg(long)]
    clip: String,
    /// The winning side.
    #[arg(long, value_parser = ["a", "b", "neither"])]
    winner: String,
    /// The reasoning.
    #[arg(long)]
    why: String,
}

#[derive(Args, Debug)]
struct FreezeArgs {
    /// Scene id to freeze an envelope for.
    #[arg(long)]
    scene: String,
    /// The approved `metrics.json` to freeze from.
    #[arg(long)]
    from: PathBuf,
    /// Relative margin around each metric value.
    #[arg(long, default_value_t = DEFAULT_MARGIN)]
    margin: f64,
    /// Absolute floor added to each band half-width.
    #[arg(long, default_value_t = DEFAULT_FLOOR)]
    floor: f64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Corpus(cmd) => cmd_corpus(&cli.quality_dir, cmd),
        Command::Run(args) => cmd_run(&cli.quality_dir, args),
        Command::Ab(args) => cmd_ab(&cli.quality_dir, args),
        Command::Verdict(args) => cmd_verdict(&cli.quality_dir, args),
        Command::Freeze(args) => cmd_freeze(&cli.quality_dir, args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("scia-harness: {e}");
            ExitCode::from(2)
        }
    }
}

fn corpus_root(quality_dir: &Path) -> PathBuf {
    quality_dir.join("corpus")
}

fn cmd_corpus(quality_dir: &Path, cmd: CorpusCmd) -> Result<(), String> {
    let root = corpus_root(quality_dir);
    match cmd {
        CorpusCmd::Synth { id } => {
            let specs: Vec<_> = match &id {
                Some(id) => {
                    vec![*synth_spec(id).ok_or_else(|| format!("unknown synth clip '{id}'"))?]
                }
                None => SYNTH_SPECS.to_vec(),
            };
            for spec in &specs {
                let outcome = corpus::synth_clip(spec, &root)?;
                println!(
                    "synth {}: {} bytes, sha256 {}, {}",
                    outcome.entry.id,
                    outcome.bytes,
                    &outcome.entry.sha256[..12],
                    if outcome.committed {
                        "committed fixture"
                    } else {
                        "generated (regenerate to verify)"
                    }
                );
            }
            println!("manifest: {}", root.join("manifest.toml").display());
            Ok(())
        }
        CorpusCmd::Verify => {
            let results = corpus::verify(&root)?;
            if results.is_empty() {
                println!("corpus verify: manifest is empty");
                return Ok(());
            }
            let mut failed = 0;
            for r in &results {
                let mark = if r.ok { "ok  " } else { "FAIL" };
                println!("{mark} {}: {}", r.id, r.detail);
                if !r.ok {
                    failed += 1;
                }
            }
            if failed == 0 {
                println!("corpus verify: {} clip(s) ok", results.len());
                Ok(())
            } else {
                Err(format!("corpus verify: {failed} clip(s) failed"))
            }
        }
    }
}

fn cmd_run(quality_dir: &Path, args: RunArgs) -> Result<(), String> {
    let root = corpus_root(quality_dir);
    let resolved = clip::resolve(&args.clip, &root).map_err(|e| e.to_string())?;
    let sets = parse_sets(&args.sets)?;

    let (preset, preset_label) = match &args.preset {
        Some(path) => {
            let (p, label) = load_preset_labeled(path)?;
            (Some(p), Some(label))
        }
        None => (None, None),
    };

    let req = RunRequest {
        scene: &args.scene,
        preset,
        preset_label,
        sets: &sets,
        frames: &resolved.frames,
        source: &resolved.source,
        hop_ms: resolved.hop_ms,
        metric_params: MetricParams::default(),
    };
    let output = replay_run(&req);

    let out_dir = args.out.unwrap_or_else(|| {
        quality_dir
            .join("runs")
            .join(format!("{}__{}", args.scene, resolved.source))
    });
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    // run.jsonl
    let mut jsonl = String::new();
    for rec in &output.records {
        jsonl.push_str(&to_line(rec).map_err(|e| e.to_string())?);
        jsonl.push('\n');
    }
    let jsonl_path = out_dir.join("run.jsonl");
    std::fs::write(&jsonl_path, jsonl).map_err(|e| e.to_string())?;

    // metrics.json
    let metrics_path = out_dir.join("metrics.json");
    std::fs::write(&metrics_path, metrics_json(&output.metrics)?).map_err(|e| e.to_string())?;

    println!(
        "run: scene {} on {} — {} hops @ {:.3} ms",
        args.scene, resolved.source, output.hops, resolved.hop_ms
    );
    print_metrics(&output.metrics);
    println!("records: {}", jsonl_path.display());
    println!("metrics: {}", metrics_path.display());
    Ok(())
}

fn cmd_ab(quality_dir: &Path, args: AbArgs) -> Result<(), String> {
    let root = corpus_root(quality_dir);
    let resolved = clip::resolve(&args.clip, &root).map_err(|e| e.to_string())?;

    let metrics_for = |preset_path: &str| -> Result<Metrics, String> {
        let (preset, label) = load_preset_labeled(preset_path)?;
        let req = RunRequest {
            scene: &args.scene,
            preset: Some(preset),
            preset_label: Some(label),
            sets: &[],
            frames: &resolved.frames,
            source: &resolved.source,
            hop_ms: resolved.hop_ms,
            metric_params: MetricParams::default(),
        };
        Ok(replay_run(&req).metrics)
    };

    let a = metrics_for(&args.preset_a)?;
    let b = metrics_for(&args.preset_b)?;

    println!(
        "A/B: scene {} on {} ({} hops)",
        args.scene,
        resolved.source,
        resolved.frames.len()
    );
    println!("  A = {}", args.preset_a);
    println!("  B = {}", args.preset_b);
    println!();
    print!("{}", ab::compare_table(&a, &b));
    println!("\nLive side-by-side (run each in its own terminal):");
    let (cmd_a, cmd_b) =
        ab::paste_commands(&args.clip, &args.scene, &args.preset_a, &args.preset_b);
    println!("  {cmd_a}");
    println!("  {cmd_b}");
    Ok(())
}

fn cmd_verdict(quality_dir: &Path, args: VerdictArgs) -> Result<(), String> {
    let path = quality_dir.join("preference-log.toml");
    let v = verdict::append(&path, &args.scene, &args.clip, &args.winner, &args.why)?;
    println!(
        "recorded verdict [{}] scene {} clip {} winner {} — {}",
        v.date, v.scene, v.clip, v.winner, v.why
    );
    println!("log: {}", path.display());
    Ok(())
}

fn cmd_freeze(quality_dir: &Path, args: FreezeArgs) -> Result<(), String> {
    let text =
        std::fs::read_to_string(&args.from).map_err(|e| format!("{}: {e}", args.from.display()))?;
    let metrics: Metrics = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let source = args.from.display().to_string();
    let env = Envelope::freeze(&args.scene, &source, &metrics, args.margin, args.floor);
    let path = quality_dir
        .join("envelopes")
        .join(format!("{}.toml", args.scene));
    env.save(&path)?;
    println!(
        "froze envelope for {} ({} metrics, ±{} rel + {} abs)",
        args.scene,
        env.bands.len(),
        args.margin,
        args.floor
    );
    println!("envelope: {}", path.display());
    Ok(())
}

/// Parse `key=value` overrides into `(String, f32)` pairs.
fn parse_sets(sets: &[String]) -> Result<Vec<(String, f32)>, String> {
    let mut out = Vec::with_capacity(sets.len());
    for s in sets {
        let (k, v) = s
            .split_once('=')
            .ok_or_else(|| format!("--set '{s}' must be key=value"))?;
        let val: f32 = v
            .trim()
            .parse()
            .map_err(|_| format!("--set '{s}': '{v}' is not a number"))?;
        out.push((k.trim().to_string(), val));
    }
    Ok(out)
}

/// Serialise metrics to pretty JSON with a trailing newline (deterministic:
/// [`Metrics`]' field order is fixed).
fn metrics_json(metrics: &Metrics) -> Result<String, String> {
    let mut s = serde_json::to_string_pretty(metrics).map_err(|e| e.to_string())?;
    s.push('\n');
    Ok(s)
}

fn print_metrics(metrics: &Metrics) {
    for (name, v) in metrics.as_pairs() {
        println!("  {name:<26} {v:>14.6}");
    }
}
