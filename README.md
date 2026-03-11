# hep-ci-rust

Continuous integration runner for the hep ecosystem. Reads a `.hep-ci.yml` pipeline file, runs jobs locally or as a daemon triggered by pushes, and keeps a log of every run.

## Build

```
cargo build --release
./target/release/hep-ci --help
```

Only dependency: `chrono`.

## Commands

| command | what it does |
|---|---|
| `init` | create a starter `.hep-ci.yml` in the current repo |
| `run [file]` | run the pipeline locally right now |
| `status [n]` | show the last N runs (default 10) |
| `logs [run-id]` | print output of a run (defaults to the last one) |
| `watch` | tail the output of the currently running job |
| `history` | full run history with timing stats |
| `cancel <run-id>` | cancel a running job |
| `clean [keep]` | delete old logs, keeping the N most recent (default 20) |
| `serve [-p port]` | start the CI daemon on port 7071 (push-triggered runs) |

## Pipeline file: `.hep-ci.yml`

```yaml
name: my pipeline

on: push

jobs:
  build:
    steps:
      - name: compile
        run: gcc -o app main.c
      - name: test
        run: ./app --test

  deploy:
    needs: build        # only runs if build passes
    steps:
      - name: ship
        run: ./deploy.sh
```

`needs:` creates a dependency between jobs — if the required job fails, the dependent job is skipped.

## Storage layout

```
.hep-ci/
  runs.log          pipe-separated run history
  logs/<run-id>.log full stdout/stderr of each run
```

Run IDs are timestamps (`YYYYMMDD-HHMMSS`). Logs are plain text and can be read with any editor.

## Daemon mode

```
hep-ci serve          # listens on port 7071
hep-ci serve -p 8080  # custom port
```

The daemon triggers a pipeline run whenever it receives a push notification from `hep-server`. Runs are logged the same way as local runs.

## Ecosystem

| tool | port | purpose |
|---|---|---|
| `hep` | — | version control (92 commands) |
| `hep-server` | 7070 | repo hosting |
| `hep-ci` | 7071 | this |
| `hep-forge` | 7072 | PRs + issues |
| `hep-registry` | 7373 | package registry |
| `hep-deploy` | 7074 | deployment manager |
