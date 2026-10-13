"""Harbor agent that runs the Cortex harness itself inside each Terminal-Bench task container.

The binary under test is uploaded from the host (`binary=` agent kwarg), so a before/after
comparison runs two builds that differ only by the change being measured. The client talks to
https://llm.imabee.com over TLS with the container's trust store; nothing on the client side is
wrapped or redirected. Auth is a Cortex API key in `CORTEX_API_KEY`, the client's own variable. The key goes
into the container as a file only the agent user can read, so it never appears on a host command line
(Harbor's `env=` becomes `docker exec -e NAME=value`); the client's commands inherit it, as on any machine.
"""

import json
import re
import shlex
import urllib.request
import uuid
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths

_REMOTE_BINARY = "/installed-agent/cortex"
_SERVER = "https://llm.imabee.com"
# The server's certificate chains to this private root; the task images do not trust it. Installing it in the
# container's OS store is the project's supported path (rule 3), and verification stays on.
_DEFAULT_CA = "/etc/ca-certificates/trust-source/anchors/appsynergy-root-ca.crt"
_REMOTE_CA = "/usr/local/share/ca-certificates/cortex-server-ca.crt"
_REMOTE_KEY = "/installed-agent/api-key"
_WITH_KEY = f'CORTEX_API_KEY="$(cat {_REMOTE_KEY})"'
_OUTPUT_FILENAME = "cortex-output.ndjson"
_LOG_FILENAME = "cortex.log"
# Kills every `cortex` process and all of its descendants, as the harness's own exit does with its background
# work; left alive, a timed-out agent's training jobs starved pytorch-model-recovery's grader of its one CPU.
# Processes the model detached on purpose (a server it leaves for grading) were reparented away and survive.
# Each process found is stopped before its children are looked for, so nothing forks past the sweep.
# procps is absent from many task images, so /proc is read directly, in plain sh.
_STOP_AGENT = r"""
tree=" "
for p in /proc/[0-9]*; do
  [ "$(cat "$p/comm" 2>/dev/null)" = cortex ] && { tree="$tree${p#/proc/} "; kill -STOP "${p#/proc/}" 2>/dev/null; }
done
grew=1
while [ "$tree" != " " ] && [ $grew = 1 ]; do
  grew=0
  for p in /proc/[0-9]*; do
    n=${p#/proc/}
    case "$tree" in *" $n "*) continue ;; esac
    pp=
    { while read -r k v; do [ "$k" = PPid: ] && { pp=$v; break; }; done < "$p/status"; } 2>/dev/null
    case "$tree" in *" $pp "*) tree="$tree$n "; kill -STOP "$n" 2>/dev/null; grew=1 ;; esac
  done
done
for n in $tree; do kill -KILL "$n" 2>/dev/null; done
true
"""


class Cortex(BaseInstalledAgent):
    def __init__(
        self, *args, binary: str, label: str = "", ca_cert: str = _DEFAULT_CA, verifier_uv_cache: str = "", **kwargs
    ):
        super().__init__(*args, **kwargs)
        self._binary = Path(binary).expanduser()
        if not self._binary.is_file():
            raise FileNotFoundError(f"cortex binary not found: {self._binary}")
        self._ca_cert = Path(ca_cert)
        if not self._ca_cert.is_file():
            raise FileNotFoundError(f"server CA certificate not found: {self._ca_cert}")
        self._label = label
        # A uv cache filled with the grader's own `uvx` packages, for a grader whose download cannot finish in its
        # time limit on this link. Same versions, same tests; only the download is skipped.
        self._verifier_uv_cache = Path(verifier_uv_cache).expanduser() if verifier_uv_cache else None
        if self._verifier_uv_cache and not self._verifier_uv_cache.is_dir():
            raise FileNotFoundError(f"verifier uv cache not found: {self._verifier_uv_cache}")
        self._session_id = str(uuid.uuid4())

    @staticmethod
    def name() -> str:
        return "cortex"

    def get_version_command(self) -> str | None:
        return f"{_REMOTE_BINARY} --version"

    def parse_version(self, stdout: str) -> str:
        version = stdout.strip().removeprefix("cortex ").strip()
        return f"{version} {self._label}".strip()

    async def install(self, environment: BaseEnvironment) -> None:
        """Nothing is downloaded: every task image has bash, and the client needs nothing else from it.

        The server's CA goes into the image's own trust store when it has the tooling; images that ship no
        trust store at all (several Ubuntu 24.04 tasks) get one holding only this CA, which is also where
        `update-ca-certificates` would put it if the agent installs ca-certificates later.
        """
        await self.exec_as_root(environment, command="mkdir -p /usr/local/share/ca-certificates /etc/ssl/certs")
        await environment.upload_file(self._binary, _REMOTE_BINARY)
        await environment.upload_file(self._ca_cert, _REMOTE_CA)
        await self.exec_as_root(
            environment,
            command=(
                f"chmod 0755 {_REMOTE_BINARY} && ln -sf {_REMOTE_BINARY} /usr/local/bin/cortex && "
                "if command -v update-ca-certificates >/dev/null; then update-ca-certificates >/dev/null; "
                f"else cat {_REMOTE_CA} >> /etc/ssl/certs/ca-certificates.crt; fi"
            ),
        )
        await self.exec_as_agent(environment, command="cortex --version")
        api_key = self._get_env("CORTEX_API_KEY")
        if not api_key:
            raise ValueError("CORTEX_API_KEY must be set on the host for the cortex agent")
        await self._upload_config_text(environment, content=api_key, remote_path=_REMOTE_KEY, filename="api-key")
        await self._check_server(environment, api_key)

    async def _check_server(self, environment: BaseEnvironment, api_key: str) -> None:
        """Fail the trial in seconds, not at the agent timeout, when the container cannot reach the server.

        `cortex models` exits 0 even when the server is unreachable: it then lists compiled fallback models.
        So the default it reports must be one the server itself lists.
        """
        request = urllib.request.Request(f"{_SERVER}/v1/models", headers={"Authorization": f"Bearer {api_key}"})
        with urllib.request.urlopen(request, timeout=30) as response:
            served = {model["id"] for model in json.load(response)["data"]}
        result = await self.exec_as_agent(
            environment,
            command=f"{_WITH_KEY} cortex models </dev/null",
            env={"CORTEX_DISABLE_AUTOUPDATER": "1"},
        )
        match = re.search(r"^Default model: (\S+)", result.stdout or "", re.MULTILINE)
        default = match.group(1) if match else None
        if default not in served:
            raise RuntimeError(f"the container did not reach {_SERVER}: default model {default!r}, server lists {sorted(served)}")

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        agent_dir = EnvironmentPaths.agent_dir.as_posix()
        command = " ".join(
            [
                _WITH_KEY,
                "cortex",
                # `=` keeps an instruction that starts with "-" (pytorch-model-recovery) from parsing as a flag.
                f"--single={shlex.quote(instruction)}",
                "--always-approve",
                "--output-format streaming-json",
                f"--session-id {self._session_id}",
            ]
        )
        env = {
            "CORTEX_DISABLE_AUTOUPDATER": "1",
            "CORTEX_LOG_FILE": f"{agent_dir}/{_LOG_FILENAME}",
        }
        try:
            await self.exec_as_agent(
                environment,
                command=(
                    f"mkdir -p {agent_dir} && "
                    f"{command} </dev/null 2>&1 | stdbuf -oL tee {agent_dir}/{_OUTPUT_FILENAME}"
                ),
                env=env,
            )
        finally:
            # Harbor's agent timeout cancels this coroutine, not the process in the container: a timed-out agent
            # kept generating while its task was graded and while the next trial's agent ran. Stop it here,
            # before the GPU permit is released, together with everything it started (see _STOP_AGENT).
            try:
                await self.exec_as_root(environment, command=_STOP_AGENT)
            except Exception:
                self.logger.warning("could not stop the cortex process after the agent phase", exc_info=True)
            # Session files (chat history, events) come back with the trial logs for review.
            try:
                await self.exec_as_agent(
                    environment,
                    command=(
                        'if [ -d "$HOME/.cortex/sessions" ]; then '
                        f'rm -rf {agent_dir}/sessions && cp -R "$HOME/.cortex/sessions" {agent_dir}/sessions; '
                        "fi"
                    ),
                )
            except Exception:
                self.logger.debug("could not copy cortex session files", exc_info=True)
            # After the agent is stopped, so the agent never sees it; the grader runs as root.
            if self._verifier_uv_cache:
                await self.exec_as_root(environment, command="mkdir -p /root/.cache")
                await environment.upload_dir(self._verifier_uv_cache, "/root/.cache/uv")

    def populate_context_post_run(self, context: AgentContext) -> None:
        """Token use and spend from the harness's own terminal `end` (or `error`) line."""
        output = self.logs_dir / _OUTPUT_FILENAME
        if not output.is_file():
            return
        last = None
        for raw in output.read_text(errors="replace").splitlines():
            try:
                line = json.loads(raw)
            except ValueError:
                continue
            if isinstance(line, dict) and line.get("type") in ("end", "error"):
                last = line
        if last is None:
            return
        usage = last.get("usage") or {}
        cache_read = usage.get("cache_read_input_tokens", 0)
        if usage:
            # Harbor counts cached tokens inside the input total; the harness reports them apart.
            context.n_input_tokens = (
                usage.get("input_tokens", 0)
                + cache_read
                + usage.get("cache_creation_input_tokens", 0)
            )
            context.n_cache_tokens = cache_read
            context.n_output_tokens = usage.get("output_tokens", 0)
        if "total_cost_usd" in last:
            context.cost_usd = last["total_cost_usd"]
        context.metadata = {
            "end": last.get("type"),
            "stop_reason": last.get("stopReason"),
            "num_turns": last.get("num_turns"),
            "reasoning_tokens": usage.get("reasoning_tokens"),
            "total_tokens": usage.get("total_tokens"),
            "usage_is_incomplete": last.get("usage_is_incomplete", False),
            "cost_is_partial": last.get("cost_is_partial", False),
        }
