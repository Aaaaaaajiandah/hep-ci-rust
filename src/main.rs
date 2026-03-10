// hep-ci — continuous integration for the hep ecosystem
// Rust edition — full parity with the C version
//
// commands: init | run | status | logs | watch | serve | cancel | history | clean
// pipeline file: .hep-ci.yml
// run storage:   .hep-ci/runs.log
// log storage:   .hep-ci/logs/<run-id>.log

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::net::{TcpListener, TcpStream};
use std::thread;

use chrono::{Local, TimeZone};

const CI_DIR:  &str = ".hep-ci";
const CI_YML:  &str = ".hep-ci.yml";
const CI_PORT: u16  = 7071;

// ════════════════════════════════════════════════════════════════════════════
// YAML PARSER
// A minimal YAML parser that handles the subset used in .hep-ci.yml:
//   - key: value scalars
//   - key: (map with indented children)
//   - - list items (scalar or {name: x, run: y})
//   - # comments
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
enum YamlValue {
    Scalar(String),
    Map(Vec<(String, YamlValue)>),
    Seq(Vec<YamlValue>),
}

impl YamlValue {
    fn get(&self, key: &str) -> Option<&YamlValue> {
        if let YamlValue::Map(pairs) = self {
            pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
        } else {
            None
        }
    }

    fn as_str(&self) -> Option<&str> {
        if let YamlValue::Scalar(s) = self { Some(s) } else { None }
    }

    fn as_map(&self) -> Option<&Vec<(String, YamlValue)>> {
        if let YamlValue::Map(m) = self { Some(m) } else { None }
    }

    fn as_seq(&self) -> Option<&Vec<YamlValue>> {
        if let YamlValue::Seq(s) = self { Some(s) } else { None }
    }

    fn str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).and_then(|v| v.as_str()).unwrap_or(default)
    }
}

fn yaml_parse(text: &str) -> Result<YamlValue, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut pos = 0usize;
    parse_map(&lines, &mut pos, -1)
}

fn indent_of(line: &str) -> i32 {
    line.chars().take_while(|c| *c == ' ').count() as i32
}

fn strip_comment(line: &str) -> &str {
    // strip # comments — but only when preceded by space or at start
    let bytes = line.as_bytes();
    let mut in_sq = false; let mut in_dq = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' if !in_dq => in_sq = !in_sq,
            b'"'  if !in_sq => in_dq = !in_dq,
            b'#'  if !in_sq && !in_dq && (i == 0 || bytes[i-1] == b' ') => {
                return line[..i].trim_end();
            }
            _ => {}
        }
    }
    line.trim_end()
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) ||
       (s.starts_with('\'') && s.ends_with('\'')) {
        &s[1..s.len()-1]
    } else {
        s
    }
}

// parse a map at indent > parent_indent
fn parse_map(lines: &[&str], pos: &mut usize, parent_indent: i32) -> Result<YamlValue, String> {
    let mut pairs: Vec<(String, YamlValue)> = Vec::new();

    while *pos < lines.len() {
        let raw = lines[*pos];
        let line = strip_comment(raw);
        if line.is_empty() { *pos += 1; continue; }

        let ind = indent_of(line);
        if ind <= parent_indent { break; }

        let content = line[ind as usize..].trim_end();

        if content.starts_with('-') {
            // this level is actually a sequence
            let seq = parse_seq(lines, pos, parent_indent)?;
            // wrap in unnamed map entry for caller to use directly
            return Ok(seq);
        }

        // key: value or key: (children follow)
        if let Some(colon_pos) = content.find(':') {
            let key = content[..colon_pos].trim().to_string();
            let rest = content[colon_pos+1..].trim();
            let rest = unquote(rest).to_string();

            *pos += 1;

            if rest.is_empty() {
                // value is on next lines
                // peek at next non-empty line to see if it's a seq or map
                let next_ind = peek_indent(lines, *pos);
                if next_ind > ind {
                    let raw_next = peek_content(lines, *pos);
                    if raw_next.starts_with('-') {
                        let val = parse_seq(lines, pos, ind)?;
                        pairs.push((key, val));
                    } else {
                        let val = parse_map(lines, pos, ind)?;
                        pairs.push((key, val));
                    }
                } else {
                    pairs.push((key, YamlValue::Scalar(String::new())));
                }
            } else {
                pairs.push((key, YamlValue::Scalar(rest)));
            }
        } else {
            *pos += 1; // skip unrecognized
        }
    }

    Ok(YamlValue::Map(pairs))
}

fn parse_seq(lines: &[&str], pos: &mut usize, parent_indent: i32) -> Result<YamlValue, String> {
    let mut items: Vec<YamlValue> = Vec::new();

    while *pos < lines.len() {
        let raw = lines[*pos];
        let line = strip_comment(raw);
        if line.is_empty() { *pos += 1; continue; }

        let ind = indent_of(line);
        if ind <= parent_indent { break; }

        let content = line[ind as usize..].trim_end();

        if !content.starts_with('-') { break; }

        let item_text = content[1..].trim();
        *pos += 1;

        if item_text.is_empty() {
            // children follow on next lines
            let val = parse_map(lines, pos, ind)?;
            items.push(val);
        } else if let Some(colon_pos) = item_text.find(':') {
            // inline map: "- key: val" — could have more keys following
            let key = item_text[..colon_pos].trim().to_string();
            let rest = unquote(item_text[colon_pos+1..].trim()).to_string();
            let mut inline_pairs: Vec<(String, YamlValue)> = Vec::new();
            inline_pairs.push((key, YamlValue::Scalar(rest)));
            // collect any additional k: v lines at same indent
            let item_ind = ind + 2;
            while *pos < lines.len() {
                let nl = strip_comment(lines[*pos]);
                if nl.is_empty() { *pos += 1; continue; }
                let ni = indent_of(nl);
                if ni < item_ind { break; }
                let nc = nl[ni as usize..].trim_end();
                if nc.starts_with('-') { break; }
                if let Some(cp) = nc.find(':') {
                    let k = nc[..cp].trim().to_string();
                    let v = unquote(nc[cp+1..].trim()).to_string();
                    inline_pairs.push((k, YamlValue::Scalar(v)));
                    *pos += 1;
                } else { break; }
            }
            items.push(YamlValue::Map(inline_pairs));
        } else {
            items.push(YamlValue::Scalar(item_text.to_string()));
        }
    }

    Ok(YamlValue::Seq(items))
}

fn peek_indent(lines: &[&str], from: usize) -> i32 {
    for i in from..lines.len() {
        let l = strip_comment(lines[i]);
        if !l.is_empty() { return indent_of(l); }
    }
    -1
}

fn peek_content(lines: &[&str], from: usize) -> &str {
    for i in from..lines.len() {
        let l = strip_comment(lines[i]);
        if !l.is_empty() {
            let ind = indent_of(l) as usize;
            return l[ind..].trim_end();
        }
    }
    ""
}

// ════════════════════════════════════════════════════════════════════════════
// PIPELINE DATA STRUCTURES
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct Step {
    name: String,
    run:  String,
}

#[derive(Debug, Clone)]
struct Job {
    name:    String,
    needs:   Vec<String>,
    steps:   Vec<Step>,
    env:     HashMap<String, String>,
    workdir: String,
    timeout: u64, // seconds, 0 = no timeout
}

#[derive(Debug, Clone)]
struct Pipeline {
    name:     String,
    on_event: String,
    jobs:     Vec<Job>,
}

fn pipeline_parse(yml_path: &Path) -> Result<Pipeline, String> {
    let text = fs::read_to_string(yml_path)
        .map_err(|e| format!("cannot open '{}': {}", yml_path.display(), e))?;

    let doc = yaml_parse(&text)?;

    let name = doc.str_or("name", "pipeline").to_string();
    let on_event = doc.str_or("on", "push").to_string();

    let mut jobs = Vec::new();

    let jobs_node = doc.get("jobs").ok_or("no 'jobs' section found")?;
    let job_pairs = jobs_node.as_map().ok_or("'jobs' must be a map")?;

    for (job_name, job_node) in job_pairs {
        let workdir = job_node.str_or("workdir", "").to_string();
        let timeout: u64 = job_node.str_or("timeout", "0").parse().unwrap_or(0);

        // needs
        let mut needs = Vec::new();
        if let Some(n) = job_node.get("needs") {
            match n {
                YamlValue::Scalar(s) => needs.push(s.clone()),
                YamlValue::Seq(items) => {
                    for item in items {
                        if let YamlValue::Scalar(s) = item { needs.push(s.clone()); }
                    }
                }
                _ => {}
            }
        }

        // env
        let mut env = HashMap::new();
        if let Some(e) = job_node.get("env") {
            if let Some(pairs) = e.as_map() {
                for (k, v) in pairs {
                    if let YamlValue::Scalar(val) = v {
                        env.insert(k.clone(), val.clone());
                    }
                }
            }
        }

        // steps
        let mut steps = Vec::new();
        if let Some(steps_node) = job_node.get("steps") {
            if let Some(seq) = steps_node.as_seq() {
                for (si, step) in seq.iter().enumerate() {
                    match step {
                        YamlValue::Scalar(cmd) => {
                            steps.push(Step {
                                name: format!("step {}", si+1),
                                run:  cmd.clone(),
                            });
                        }
                        YamlValue::Map(_) => {
                            let sname = step.str_or("name", &format!("step {}", si+1)).to_string();
                            let srun  = step.str_or("run", "").to_string();
                            steps.push(Step { name: sname, run: srun });
                        }
                        _ => {}
                    }
                }
            }
        }

        jobs.push(Job { name: job_name.clone(), needs, steps, env, workdir, timeout });
    }

    Ok(Pipeline { name, on_event, jobs })
}

// ════════════════════════════════════════════════════════════════════════════
// PIPELINE RUNNER
// ════════════════════════════════════════════════════════════════════════════

fn run_step(step: &Step, job: &Job, repo_root: &Path, log: &mut File) -> i32 {
    let header = format!("\n── step: {} ──\n$ {}\n", step.name, step.run);
    let _ = log.write_all(header.as_bytes());

    let workdir = if job.workdir.is_empty() {
        repo_root.to_path_buf()
    } else {
        repo_root.join(&job.workdir)
    };

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&step.run)
       .current_dir(&workdir)
       .env("HEP_CI", "1")
       .env("HEP_REPO", repo_root.to_string_lossy().as_ref());

    for (k, v) in &job.env { cmd.env(k, v); }

    // capture combined stdout+stderr
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let code = match cmd.spawn() {
        Ok(mut child) => {
            // stream stdout
            if let Some(stdout) = child.stdout.take() {
                let mut buf = Vec::new();
                let mut reader = BufReader::new(stdout);
                let _ = reader.read_to_end(&mut buf);
                let _ = log.write_all(&buf);
                // also print to terminal
                let _ = io::stdout().write_all(&buf);
            }
            if let Some(stderr) = child.stderr.take() {
                let mut buf = Vec::new();
                let mut reader = BufReader::new(stderr);
                let _ = reader.read_to_end(&mut buf);
                let _ = log.write_all(&buf);
                let _ = io::stderr().write_all(&buf);
            }
            match child.wait() {
                Ok(status) => status.code().unwrap_or(1),
                Err(_) => 1,
            }
        }
        Err(e) => {
            let msg = format!("failed to spawn: {}\n", e);
            let _ = log.write_all(msg.as_bytes());
            127
        }
    };

    let _ = log.write_all(format!("── exit: {} ──\n", code).as_bytes());
    code
}

fn pipeline_run(p: &Pipeline, repo_root: &Path, log_path: &Path, run_id: &str) -> i32 {
    let mut log = match File::create(log_path) {
        Ok(f) => f,
        Err(e) => { eprintln!("cannot create log: {}", e); return 1; }
    };

    let now = Local::now();
    let header = format!(
        "hep-ci run {}\npipeline: {}\nstarted:  {}\nrepo:     {}\n{}\n",
        run_id, p.name, now.format("%Y-%m-%d %H:%M:%S"),
        repo_root.display(),
        "═".repeat(40)
    );
    let _ = log.write_all(header.as_bytes());

    let mut job_done:   Vec<bool> = vec![false; p.jobs.len()];
    let mut job_passed: Vec<bool> = vec![false; p.jobs.len()];
    let mut overall = 0i32;
    let mut executed = 0;
    let start = now_ts();

    // topological execution — up to njobs passes handles any valid DAG
    for _pass in 0..p.jobs.len() {
        if executed == p.jobs.len() { break; }
        for ji in 0..p.jobs.len() {
            if job_done[ji] { continue; }
            let job = &p.jobs[ji];

            // check dependencies
            let mut deps_ok = true;
            let mut should_skip = false;
            for need in &job.needs {
                let dep_idx = p.jobs.iter().position(|j| &j.name == need);
                match dep_idx {
                    None => { deps_ok = false; }
                    Some(di) => {
                        if !job_done[di] { deps_ok = false; }
                        else if !job_passed[di] {
                            // dependency failed — skip this job
                            let msg = format!("\n══ job: {} — SKIPPED (dep {} failed)\n",
                                              job.name, need);
                            let _ = log.write_all(msg.as_bytes());
                            job_done[ji] = true;
                            job_passed[ji] = false;
                            executed += 1;
                            should_skip = true;
                            break;
                        }
                    }
                }
            }
            if should_skip { continue; }
            if !deps_ok { continue; }

            // run this job
            let jhdr = format!("\n══ job: {} ══\n", job.name);
            let _ = log.write_all(jhdr.as_bytes());
            println!("{}", jhdr.trim());

            let mut job_ok = true;
            for step in &job.steps {
                let rc = run_step(step, job, repo_root, &mut log);
                if rc != 0 {
                    job_ok = false;
                    let msg = format!("step '{}' failed (exit {}) — stopping job\n",
                                      step.name, rc);
                    let _ = log.write_all(msg.as_bytes());
                    break;
                }
            }

            job_done[ji]   = true;
            job_passed[ji] = job_ok;
            if !job_ok { overall = 1; }
            executed += 1;

            let result = format!("══ job: {} — {} ══\n",
                                  job.name, if job_ok { "PASS" } else { "FAIL" });
            let _ = log.write_all(result.as_bytes());
            println!("{}", result.trim());
        }
    }

    let end = now_ts();
    let footer = format!(
        "\n{}\nfinished: {}\nduration: {}s\nresult:   {}\n",
        "═".repeat(40),
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        end - start,
        if overall == 0 { "PASS" } else { "FAIL" }
    );
    let _ = log.write_all(footer.as_bytes());
    overall
}

// ════════════════════════════════════════════════════════════════════════════
// RUN STORAGE
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
enum CiStatus { Running, Pass, Fail, Cancelled }

impl CiStatus {
    fn as_str(&self) -> &str {
        match self {
            CiStatus::Running   => "running",
            CiStatus::Pass      => "pass",
            CiStatus::Fail      => "fail",
            CiStatus::Cancelled => "cancelled",
        }
    }
    fn color(&self) -> &str {
        match self {
            CiStatus::Pass      => "\x1b[32m",
            CiStatus::Fail      => "\x1b[31m",
            CiStatus::Running   => "\x1b[33m",
            CiStatus::Cancelled => "\x1b[90m",
        }
    }
    fn from_int(n: i32) -> Self {
        match n {
            0 => CiStatus::Running,
            1 => CiStatus::Pass,
            2 => CiStatus::Fail,
            3 => CiStatus::Cancelled,
            _ => CiStatus::Fail,
        }
    }
    fn to_int(&self) -> i32 {
        match self {
            CiStatus::Running   => 0,
            CiStatus::Pass      => 1,
            CiStatus::Fail      => 2,
            CiStatus::Cancelled => 3,
        }
    }
}

#[derive(Debug, Clone)]
struct Run {
    run_id:       String,
    pipeline:     String,
    commit:       String,
    branch:       String,
    triggered_by: String,
    started:      u64,
    finished:     u64,
    status:       CiStatus,
    log_path:     String,
}

impl Run {
    fn to_line(&self) -> String {
        format!("{}|{}|{}|{}|{}|{}|{}|{}|{}\n",
            self.run_id, self.pipeline, self.commit, self.branch,
            self.triggered_by, self.started, self.finished,
            self.status.to_int(), self.log_path)
    }

    fn from_line(line: &str) -> Option<Self> {
        let parts: Vec<&str> = line.trim().splitn(9, '|').collect();
        if parts.len() < 8 { return None; }
        Some(Run {
            run_id:       parts[0].to_string(),
            pipeline:     parts[1].to_string(),
            commit:       parts[2].to_string(),
            branch:       parts[3].to_string(),
            triggered_by: parts[4].to_string(),
            started:      parts[5].parse().unwrap_or(0),
            finished:     parts[6].parse().unwrap_or(0),
            status:       CiStatus::from_int(parts[7].parse().unwrap_or(2)),
            log_path:     parts.get(8).unwrap_or(&"").to_string(),
        })
    }
}

fn run_save(ci_dir: &Path, run: &Run) -> io::Result<()> {
    fs::create_dir_all(ci_dir)?;
    let path = ci_dir.join("runs.log");
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(run.to_line().as_bytes())
}

fn run_list(ci_dir: &Path) -> Vec<Run> {
    let path = ci_dir.join("runs.log");
    let content = fs::read_to_string(path).unwrap_or_default();
    content.lines().filter_map(|l| Run::from_line(l)).collect()
}

fn run_load(ci_dir: &Path, run_id: &str) -> Option<Run> {
    // return the LAST entry for this run_id (most recent status)
    run_list(ci_dir).into_iter()
        .filter(|r| r.run_id == run_id)
        .last()
}

fn dedup_runs(runs: &[Run]) -> Vec<Run> {
    // for each run_id keep only the last entry (most recent status update)
    let mut seen = Vec::new();
    let mut result = Vec::new();
    for run in runs.iter().rev() {
        if !seen.contains(&run.run_id) {
            seen.push(run.run_id.clone());
            result.push(run.clone());
        }
    }
    result.reverse();
    result
}

fn run_list_dedup(ci_dir: &Path) -> Vec<Run> {
    dedup_runs(&run_list(ci_dir))
}

fn fmt_duration(secs: u64) -> String {
    if secs < 60 { format!("{}s", secs) }
    else { format!("{}m{}s", secs/60, secs%60) }
}

// ════════════════════════════════════════════════════════════════════════════
// REPO HELPERS
// ════════════════════════════════════════════════════════════════════════════

fn find_repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for _ in 0..20 {
        if dir.join(".hep").exists() { return dir; }
        if !dir.pop() { break; }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn read_head(repo_root: &Path) -> (String, String) {
    let head_path = repo_root.join(".hep/HEAD");
    let content = fs::read_to_string(&head_path).unwrap_or_default();
    let content = content.trim();

    if let Some(ref_path) = content.strip_prefix("ref: ") {
        let branch = ref_path.rsplit('/').next().unwrap_or("main").to_string();
        let sha_path = repo_root.join(".hep").join(ref_path);
        let sha = fs::read_to_string(sha_path).unwrap_or_default().trim().to_string();
        (sha, branch)
    } else {
        (content.to_string(), "HEAD".to_string())
    }
}

fn now_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn make_run_id() -> String {
    Local::now().format("%Y%m%d-%H%M%S").to_string()
}

fn ci_dir_for(repo_root: &Path) -> PathBuf {
    repo_root.join(CI_DIR)
}

// ════════════════════════════════════════════════════════════════════════════
// COMMANDS
// ════════════════════════════════════════════════════════════════════════════

// ── init ──────────────────────────────────────────────────────────────────

fn cmd_init(_args: &[String]) {
    if Path::new(CI_YML).exists() {
        println!("init: {} already exists", CI_YML);
        return;
    }
    let template = r#"name: my pipeline

on: push

jobs:
  build:
    steps:
      - name: compile
        run: echo 'add your build command here'
      - name: test
        run: echo 'add your test command here'

  # example: job that only runs after build passes
  # deploy:
  #   needs: build
  #   steps:
  #     - name: ship
  #       run: ./deploy.sh
"#;
    fs::write(CI_YML, template).unwrap();
    println!("init: created {}", CI_YML);
    println!("      edit it, then run 'hep-ci run' to test locally");
    println!("      hep-ci serve  starts the daemon for push-triggered runs");
}

// ── run ───────────────────────────────────────────────────────────────────

fn cmd_run(args: &[String]) {
    let yml_path = args.first().map(|s| Path::new(s.as_str()).to_path_buf())
        .unwrap_or_else(|| Path::new(CI_YML).to_path_buf());

    let repo_root = find_repo_root();
    let ci_dir = ci_dir_for(&repo_root);
    fs::create_dir_all(ci_dir.join("logs")).unwrap();

    let pipeline = match pipeline_parse(&yml_path) {
        Ok(p) => p,
        Err(e) => { eprintln!("run: {}", e); std::process::exit(1); }
    };

    let (commit, branch) = read_head(&repo_root);
    let run_id = make_run_id();
    let log_path = ci_dir.join("logs").join(format!("{}.log", run_id));

    println!("hep-ci run");
    println!("pipeline : {}", pipeline.name);
    println!("jobs     : {}", pipeline.jobs.len());
    println!("repo     : {}", repo_root.display());
    println!("run id   : {}", run_id);
    println!("log      : {}\n", log_path.display());

    let run = Run {
        run_id:       run_id.clone(),
        pipeline:     pipeline.name.clone(),
        commit:       if commit.is_empty() { "none".into() } else { commit[..commit.len().min(40)].to_string() },
        branch:       if branch.is_empty() { "unknown".into() } else { branch },
        triggered_by: "manual".into(),
        started:      now_ts(),
        finished:     0,
        status:       CiStatus::Running,
        log_path:     log_path.to_string_lossy().to_string(),
    };
    run_save(&ci_dir, &run).unwrap();

    let start = now_ts();
    let rc = pipeline_run(&pipeline, &repo_root, &log_path, &run_id);
    let end = now_ts();

    let final_run = Run {
        finished: end,
        status: if rc == 0 { CiStatus::Pass } else { CiStatus::Fail },
        ..run
    };
    run_save(&ci_dir, &final_run).unwrap();

    let col = final_run.status.color();
    let label = if rc == 0 { "PASS ✓" } else { "FAIL ✗" };
    println!("\n{}{}\x1b[0m — {}s", col, label, end - start);
    if rc != 0 {
        println!("run 'hep-ci logs {}' to see output", run_id);
    }
    std::process::exit(rc);
}

// ── status ────────────────────────────────────────────────────────────────

fn cmd_status(args: &[String]) {
    let limit: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(10);
    let repo_root = find_repo_root();
    let ci_dir = ci_dir_for(&repo_root);
    let runs = run_list_dedup(&ci_dir);

    if runs.is_empty() {
        println!("status: no runs yet — use 'hep-ci run' to start one");
        return;
    }

    println!("{:<20}  {:<8}  {:<12}  {:<10}  {:<8}  {}",
        "run id", "status", "pipeline", "branch", "duration", "commit");
    println!("{:<20}  {:<8}  {:<12}  {:<10}  {:<8}  {}",
        "─".repeat(20), "─".repeat(8), "─".repeat(12),
        "─".repeat(10), "─".repeat(8), "─".repeat(7));

    for run in runs.iter().rev().take(limit) {
        let dur = if run.finished > run.started { run.finished - run.started } else { 0 };
        let commit_short = if run.commit.len() >= 7 { &run.commit[..7] } else { &run.commit };
        let col = run.status.color();
        println!("{:<20}  {}{:<8}\x1b[0m  {:<12}  {:<10}  {:<8}  {}",
            run.run_id, col, run.status.as_str(),
            run.pipeline, run.branch, fmt_duration(dur), commit_short);
    }
}

// ── logs ──────────────────────────────────────────────────────────────────

fn cmd_logs(args: &[String]) {
    let repo_root = find_repo_root();
    let ci_dir = ci_dir_for(&repo_root);

    let run_id = if let Some(id) = args.first() {
        id.clone()
    } else {
        // most recent run
        let runs = run_list_dedup(&ci_dir);
        match runs.last() {
            Some(r) => r.run_id.clone(),
            None => { println!("logs: no runs yet"); return; }
        }
    };

    let run = match run_load(&ci_dir, &run_id) {
        Some(r) => r,
        None => { eprintln!("logs: run '{}' not found", run_id); return; }
    };

    if run.log_path.is_empty() {
        eprintln!("logs: no log path recorded for run '{}'", run_id); return;
    }

    match fs::read_to_string(&run.log_path) {
        Ok(content) => print!("{}", content),
        Err(e) => eprintln!("logs: cannot read log '{}': {}", run.log_path, e),
    }
}

// ── watch ─────────────────────────────────────────────────────────────────

fn cmd_watch(_args: &[String]) {
    let repo_root = find_repo_root();
    let ci_dir = ci_dir_for(&repo_root);

    let runs = run_list_dedup(&ci_dir);
    let active = runs.iter().rev().find(|r| r.status == CiStatus::Running);

    let run = match active {
        Some(r) => r.clone(),
        None => { println!("watch: no running jobs"); return; }
    };

    println!("watch: tailing run {} (Ctrl+C to stop)\n", run.run_id);

    let log_path = Path::new(&run.log_path);
    let mut offset: u64 = 0;

    loop {
        // read new bytes
        if let Ok(mut f) = File::open(log_path) {
            use std::io::Seek;
            let _ = f.seek(std::io::SeekFrom::Start(offset));
            let mut buf = Vec::new();
            let n = f.read_to_end(&mut buf).unwrap_or(0);
            if n > 0 {
                let _ = io::stdout().write_all(&buf);
                offset += n as u64;
            }
        }

        // check if still running
        if let Some(cur) = run_load(&ci_dir, &run.run_id) {
            if cur.status != CiStatus::Running {
                // drain remaining
                if let Ok(mut f) = File::open(log_path) {
                    use std::io::Seek;
                    let _ = f.seek(std::io::SeekFrom::Start(offset));
                    let mut buf = Vec::new();
                    let _ = f.read_to_end(&mut buf);
                    let _ = io::stdout().write_all(&buf);
                }
                let col = cur.status.color();
                let label = if cur.status == CiStatus::Pass { "PASS ✓" } else { "FAIL ✗" };
                println!("\n{}{}\x1b[0m", col, label);
                break;
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

// ── history ───────────────────────────────────────────────────────────────

fn cmd_history(_args: &[String]) {
    let repo_root = find_repo_root();
    let ci_dir = ci_dir_for(&repo_root);
    let runs = run_list_dedup(&ci_dir);

    if runs.is_empty() { println!("history: no runs"); return; }

    println!("full run history:\n");
    let (mut pass, mut fail, mut total) = (0u64, 0u64, 0u64);
    let mut total_dur = 0u64;

    for run in runs.iter().rev() {
        let dur = if run.finished > run.started { run.finished - run.started } else { 0 };
        let ts = Local.timestamp_opt(run.started as i64, 0)
            .single().map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let col = run.status.color();
        println!("  {}{:<8}\x1b[0m  {}  {}  branch:{:<12}  {}",
            col, run.status.as_str(), run.run_id, ts, run.branch, run.pipeline);

        match run.status {
            CiStatus::Pass => { pass += 1; total += 1; total_dur += dur; }
            CiStatus::Fail => { fail += 1; total += 1; total_dur += dur; }
            _ => {}
        }
    }

    print!("\n{} total  |  {} pass  |  {} fail", total, pass, fail);
    if total > 0 {
        print!("  |  avg duration: {}", fmt_duration(total_dur / total));
    }
    println!();
}

// ── cancel ────────────────────────────────────────────────────────────────

fn cmd_cancel(args: &[String]) {
    if args.is_empty() { eprintln!("cancel: Usage: hep-ci cancel <run-id>"); return; }
    let repo_root = find_repo_root();
    let ci_dir = ci_dir_for(&repo_root);

    let run = match run_load(&ci_dir, &args[0]) {
        Some(r) => r,
        None => { eprintln!("cancel: run '{}' not found", args[0]); return; }
    };

    if run.status != CiStatus::Running {
        println!("cancel: run '{}' is not running (status: {})", args[0], run.status.as_str());
        return;
    }

    // write cancel marker
    let _ = fs::write(ci_dir.join(format!("cancel_{}", args[0])), "cancel\n");

    let cancelled = Run {
        status: CiStatus::Cancelled,
        finished: now_ts(),
        ..run
    };
    run_save(&ci_dir, &cancelled).unwrap();
    println!("cancel: run '{}' marked for cancellation", args[0]);
}

// ── clean ─────────────────────────────────────────────────────────────────

fn cmd_clean(args: &[String]) {
    let keep: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(20);
    let repo_root = find_repo_root();
    let ci_dir = ci_dir_for(&repo_root);
    let runs = run_list_dedup(&ci_dir);

    if runs.len() <= keep {
        println!("clean: only {} runs, nothing to clean (keeping {})", runs.len(), keep);
        return;
    }

    let to_delete = runs.len() - keep;
    let mut deleted = 0;

    for run in runs.iter().take(to_delete) {
        if !run.log_path.is_empty() {
            let _ = fs::remove_file(&run.log_path);
        }
        deleted += 1;
    }

    // rewrite runs.log with only the kept runs
    let keep_runs = &runs[to_delete..];
    let path = ci_dir.join("runs.log");
    let mut f = File::create(path).unwrap();
    for run in keep_runs {
        f.write_all(run.to_line().as_bytes()).unwrap();
    }

    println!("clean: removed {} old runs, kept {}", deleted, keep);
}

// ── serve ─────────────────────────────────────────────────────────────────

fn handle_webhook(mut stream: TcpStream, repos_dir: String) {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    if n == 0 { return; }
    let request = String::from_utf8_lossy(&buf[..n]).to_string();

    // parse body: repo=name&commit=sha&branch=main
    let body = request.find("\r\n\r\n")
        .map(|i| &request[i+4..])
        .or_else(|| request.find("\n\n").map(|i| &request[i+2..]))
        .unwrap_or("");

    let mut repo_name = String::new();
    let mut commit    = String::new();
    let mut branch    = "main".to_string();

    for part in body.split('&') {
        if let Some(v) = part.strip_prefix("repo=")   { repo_name = v.trim().to_string(); }
        if let Some(v) = part.strip_prefix("commit=") { commit    = v.trim().to_string(); }
        if let Some(v) = part.strip_prefix("branch=") { branch    = v.trim().to_string(); }
    }

    // respond immediately
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    drop(stream);

    if repo_name.is_empty() { return; }

    let repo_root = PathBuf::from(&repos_dir).join(&repo_name);
    let yml_path  = repo_root.join(CI_YML);

    if !yml_path.exists() {
        println!("serve: repo '{}' has no {} — skipping", repo_name, CI_YML);
        return;
    }

    // run pipeline in a thread
    thread::spawn(move || {
        let pipeline = match pipeline_parse(&yml_path) {
            Ok(p) => p,
            Err(e) => { eprintln!("serve: parse error for {}: {}", repo_name, e); return; }
        };

        let ci_dir = ci_dir_for(&repo_root);
        fs::create_dir_all(ci_dir.join("logs")).unwrap();

        let run_id   = make_run_id();
        let log_path = ci_dir.join("logs").join(format!("{}.log", run_id));

        let run = Run {
            run_id:       run_id.clone(),
            pipeline:     pipeline.name.clone(),
            commit:       if commit.is_empty() { "none".into() } else { commit.clone() },
            branch:       branch.clone(),
            triggered_by: "push".into(),
            started:      now_ts(),
            finished:     0,
            status:       CiStatus::Running,
            log_path:     log_path.to_string_lossy().to_string(),
        };
        run_save(&ci_dir, &run).unwrap();

        println!("serve: running pipeline for '{}' (run {})", repo_name, run_id);

        let rc = pipeline_run(&pipeline, &repo_root, &log_path, &run_id);
        let final_run = Run {
            finished: now_ts(),
            status: if rc == 0 { CiStatus::Pass } else { CiStatus::Fail },
            ..run
        };
        run_save(&ci_dir, &final_run).unwrap();
        println!("serve: run {} {}", run_id, if rc == 0 { "PASS" } else { "FAIL" });
    });
}

fn cmd_serve(args: &[String]) {
    let mut port      = CI_PORT;
    let mut repos_dir = "./repos".to_string();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => { i += 1; if i < args.len() { port = args[i].parse().unwrap_or(CI_PORT); } }
            "-d" => { i += 1; if i < args.len() { repos_dir = args[i].clone(); } }
            _ => {}
        }
        i += 1;
    }

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
        .unwrap_or_else(|e| { eprintln!("serve: bind failed: {}", e); std::process::exit(1); });

    println!("hep-ci serve listening on :{}", port);
    println!("repos dir: {}", repos_dir);
    println!("waiting for push webhooks from hep-server...\n");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let rd = repos_dir.clone();
                thread::spawn(move || handle_webhook(s, rd));
            }
            Err(e) => eprintln!("serve: accept error: {}", e),
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// HELP + DISPATCH
// ════════════════════════════════════════════════════════════════════════════

fn usage() {
    println!(concat!(
        "hep-ci — continuous integration for the hep ecosystem\n\n",
        "usage: hep-ci <command> [options]\n\n",
        "commands:\n",
        "  init              create .hep-ci.yml pipeline file\n",
        "  run [file]        run pipeline locally now\n",
        "  status [n]        show last N runs (default 10)\n",
        "  logs [run-id]     show output of a run (default: last)\n",
        "  watch             tail output of currently running job\n",
        "  serve [-p port]   start CI daemon for push-triggered runs\n",
        "  cancel <run-id>   cancel a running job\n",
        "  history           full run history with stats\n",
        "  clean [keep]      delete old logs, keep N most recent (default 20)\n\n",
        "pipeline file: .hep-ci.yml\n\n",
        "example .hep-ci.yml:\n",
        "  name: build\n",
        "  on: push\n",
        "  jobs:\n",
        "    build:\n",
        "      steps:\n",
        "        - name: compile\n",
        "          run: gcc -o app main.c\n",
        "        - name: test\n",
        "          run: ./app --test\n\n",
        "ecosystem:\n",
        "  hep         version control  (92 commands)\n",
        "  hep-server  repo server      (git HTTP + web UI)\n",
        "  hep-ci      this\n",
    ));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() { usage(); return; }

    let rest = if args.len() > 1 { args[1..].to_vec() } else { vec![] };

    match args[0].as_str() {
        "init"     => cmd_init(&rest),
        "run"      => cmd_run(&rest),
        "status"   => cmd_status(&rest),
        "logs"     => cmd_logs(&rest),
        "watch"    => cmd_watch(&rest),
        "serve"    => cmd_serve(&rest),
        "cancel"   => cmd_cancel(&rest),
        "history"  => cmd_history(&rest),
        "clean"    => cmd_clean(&rest),
        "--help" | "-h" => usage(),
        "--version"     => println!("hep-ci v1.0.0 (Rust edition)"),
        cmd => {
            eprintln!("hep-ci: unknown command '{}'\nrun 'hep-ci --help' for usage", cmd);
            std::process::exit(1);
        }
    }
}
