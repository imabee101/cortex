#!/bin/bash
# Prepares the Terminal-Bench working directory ($CORTEX_TBENCH_HOME, default ~/data/cortex-tbench): Harbor, the task
# set, and the pre-filled uv cache that `baseline.sh` can hand a grader (VERIFIER_UV_CACHE). Safe to rerun.
set -euo pipefail
work=${CORTEX_TBENCH_HOME:-$HOME/data/cortex-tbench}
mkdir -p "$work" && cd "$work"

if [ ! -x .venv/bin/harbor ]; then
  uv venv -q -p 3.13 .venv
  uv pip install -q -p .venv/bin/python "harbor==0.23.0"
fi
[ -d dataset/terminal-bench-2 ] || .venv/bin/harbor download terminal-bench/terminal-bench-2 -o dataset
# The registry serves @latest; record what was fetched so two runs can be checked for the same tasks.
(cd dataset/terminal-bench-2 && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1) > dataset.sha256

# pytorch-model-recovery's grader downloads torch with uv 0.9.5 inside its time limit, which this link cannot meet.
# The cache holds exactly the packages that grader asks for, fetched by the same uv version.
uvdir=uv-0.9.5
if [ ! -x $uvdir/uv-x86_64-unknown-linux-gnu/uv ]; then
  mkdir -p $uvdir && (cd $uvdir &&
    curl -sSLO https://github.com/astral-sh/uv/releases/download/0.9.5/uv-x86_64-unknown-linux-gnu.tar.gz &&
    curl -sSLO https://github.com/astral-sh/uv/releases/download/0.9.5/uv-x86_64-unknown-linux-gnu.tar.gz.sha256 &&
    sha256sum -c uv-x86_64-unknown-linux-gnu.tar.gz.sha256 && tar xzf uv-x86_64-unknown-linux-gnu.tar.gz)
fi
cache=uv-cache
if [ ! -d $cache/cache ]; then
  mkdir -p $cache/cache $cache/python $cache/work
  (cd $cache/work && UV_CACHE_DIR=$work/$cache/cache UV_PYTHON_INSTALL_DIR=$work/$cache/python UV_PYTHON_PREFERENCE=only-managed \
    "$work/$uvdir/uv-x86_64-unknown-linux-gnu/uvx" -p 3.13 -w pytest==8.4.1 -w torch==2.7.1 -w pytest-json-ctrf==0.3.5 pytest --version)
fi
echo "ready: $work (tasks $(cat dataset.sha256))"
