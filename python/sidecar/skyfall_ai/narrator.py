"""LLM contextualizer (phase 5, root cause in phase 6).

On a genuine AnomalyEvent (already debounced by the detectors) the narrator sends
a compact prompt to an OpenAI-compatible `/v1/chat/completions` endpoint and asks
for a *verdict* as strict JSON:

    {severity, headline, root_cause, explanation, recommended_action, confidence}

`root_cause` is the most likely cause given the dossier's causal block (trends,
co-moving metrics, resource hogs). It is advisory: the model is told to name a
cause only when the evidence supports one and to say so plainly when it does not.

Default endpoint is a local Ollama (`http://localhost:11434`) for privacy and zero
cost; any OpenAI-compatible cloud endpoint works by pointing `base_url` at it (e.g.
`https://api.openai.com/v1`) and setting `api_key`. Calls are trigger-only,
throttled per (kind, scope), and silently disable for the session after a few
consecutive failures so a dead endpoint never spams the logs or stalls the feed
(beyond the caller-supplied timeout).

`DEFAULT_MODEL` is deliberately the 3B: consumer laptops are usually CPU-only (or have
weak integrated graphics), where a 7B model cannot sustain the ~10 tok/s needed to
finish a verdict inside `DEFAULT_TIMEOUT_S` and every call would time out. Users with a
discrete GPU can raise this in Settings; the trade-off is documented there.
"""

from __future__ import annotations

import json
import logging
import queue
import threading
import time
import urllib.error
import urllib.request
from functools import lru_cache
from typing import Any, Callable, Optional

from .events import AnomalyEvent

log = logging.getLogger("skyfall_ai")

#: Default local model. Small enough to run CPU-only inside the timeout.
DEFAULT_MODEL = "qwen2.5:3b"
#: The verdict is a small JSON blob; the prompt caps each field, so this is generous.
MAX_TOKENS = 256
#: Generous because the call pays Ollama's cold model load when the model is not
#: kept resident between verdicts (see `DEFAULT_KEEP_ALIVE_S`). Measured ~93s for
#: qwen2.5:3b on a CPU-only i7-13700H (20 threads, no discrete GPU). A tighter
#: value makes the very first alert fail on exactly the machines we are targeting.
DEFAULT_TIMEOUT_S = 120.0
#: How long to stay paused after a missing model before re-probing.
MODEL_RECHECK_S = 300.0
#: How long the endpoint should keep the model resident after a verdict.
#:
#: Ollama's own default is 5 minutes, *renewed on every request*, and its runner
#: (`llama-server`) holds the whole model in RAM plus several cores while it
#: generates. A monitor that narrates on every alert therefore keeps a multi-GB
#: runner alive almost permanently, for a verdict nobody is waiting on. Zero
#: unloads the model as soon as the verdict is in; the price is a cold model load
#: per verdict (~90s CPU-only), which is affordable only because narration runs
#: on a background thread. Raise it to trade RAM for latency.
#: Release the model as soon as a verdict is in: a resident Ollama runner costs
#: gigabytes of RAM for a verdict nobody is waiting on. See `DEFAULT_KEEP_ALIVE_S`.
DEFAULT_KEEP_ALIVE_S = 0.0
#: How long to keep it loaded for the *rest* of a burst. A machine in trouble
#: trips many detectors at once, and those verdicts arrive seconds apart: staying
#: warm between them reuses the cached prompt prefix (~40% of the prefill) and
#: skips a model load per verdict. The last call in a burst still uses
#: `keep_alive_s`, so an isolated alert leaves nothing resident.
DEFAULT_BURST_KEEP_ALIVE_S = 45.0
#: Verdicts one batch may cost. A batch is one snapshot's alerts, and the throttle
#: is per `(kind, scope)` — per *sensor* — so a laptop with five temperature probes
#: tripping together used to buy five verdicts of the same story. Three covers the
#: genuinely different problems a single snapshot can surface; `0` disables the cap.
DEFAULT_MAX_PER_BURST = 3
#: How many collapsed-alert fingerprints to remember, so dedup survives a long
#: session without growing without bound.
FINGERPRINT_MEMORY = 64
#: The `/api/tags` probe that decides whether we can ask for a keep-alive at
#: all. It only decides which route to take, so it must be quick and its failure
#: is not the narrator's problem.
ENDPOINT_PROBE_TIMEOUT_S = 3.0

#: Rough on-disk sizes, so Settings can warn before a multi-GB download.
MODEL_SIZES_HINT = {
    "qwen2.5:0.5b": "~400 MB",
    "qwen2.5:1.5b": "~1.0 GB",
    "qwen2.5:3b": "~1.9 GB",
    "qwen2.5:7b": "~4.7 GB",
    "qwen2.5:14b": "~9 GB",
}


class ModelNotInstalled(RuntimeError):
    """Endpoint is up but does not have the configured model (HTTP 404)."""

SYSTEM_PROMPT = """You are SkyFall's resident systems analyst inside a consumer AI monitor. \
An anomaly was detected on the user's machine. Using ONLY the provided anomaly details and context \
dossier, return a verdict as valid JSON with exactly these keys:
{
  "severity": "low|medium|high|critical",
  "headline": "one line, under 80 chars",
  "root_cause": "most likely cause, 1-2 sentences naming the process/subsystem and the mechanism",
  "explanation": "2-3 concrete sentences, under 300 chars",
  "recommended_action": "one short actionable step, under 150 chars",
  "confidence": 0.9
}
The dossier may include a causal block: 'trends' (which metrics are rising and over what \
window), 'also elevated' and 'rising together' (co-moving metrics), and 'resource hogs' \
(processes burning CPU/IO in that window). Use it to attribute a cause. Name the specific \
process or subsystem and the mechanism when the evidence supports it (e.g. "cargo/rustc \
compiling many crates: CPU and disk writes rising together"). If the evidence is insufficient, \
say so in root_cause rather than inventing one. Set confidence (0.0 to 1.0) to how sure you \
are. Do not invent metrics that are not in the dossier. Output the JSON object and nothing else."""

_SEVERITIES = {"low", "medium", "high", "critical"}
#: Worst first. Sorted stably, so alerts of equal severity keep detector order.
_SEVERITY_RANK = {"critical": 0, "high": 1, "medium": 2, "low": 3}


def _rank(ev: AnomalyEvent) -> int:
    """How badly an alert reads, worst first (unknown severities sort last)."""
    return _SEVERITY_RANK.get(ev.severity, len(_SEVERITY_RANK))


def _fingerprint(ev: AnomalyEvent) -> str:
    """Identity of the *story* an alert tells, used to collapse duplicates.

    The dossier's first line is machine-wide state (CPU/RAM/load/top processes),
    so two alerts that tripped on different sensors but saw the same machine carry
    byte-identical evidence — which is the normal case, since a hot CPU reports 32
    cores at once. Only the scope *class* is kept (`temp`, `disk`, `gpu`, `global`)
    because that is what decides whether one explanation covers both.

    Severity is deliberately *not* part of this: the hottest core is the one worth
    narrating, so cores that tripped at different severities must still group (the
    worst of them is chosen in `_candidates`). A sensor escalating on its own is
    still re-narrated, because that is the `(kind, scope)` throttle's business.
    """
    state = (ev.dossier or "").split("\n", 1)[0]
    return "|".join(
        (ev.kind, ev.metric, ev.scope.split(":", 1)[0], state))

Transport = Callable[[str, str, Optional[str], str, float], str]


def _strip_fences(content: str) -> str:
    start = content.find("{")
    end = content.rfind("}")
    if start >= 0 and end >= start:
        return content[start : end + 1]
    return content


def parse_verdict(content: str, fallback_severity: str) -> dict[str, Any]:
    """Coerce whatever the LLM returned into a well-formed verdict dict."""
    raw = _strip_fences(content)
    try:
        v = json.loads(raw)
    except (json.JSONDecodeError, ValueError):
        log.warning("narrator: LLM response was not JSON (%s chars)", len(content))
        return {
            "severity": fallback_severity,
            "headline": "LLM verdict unreadable",
            "root_cause": "",
            "explanation": content[:300] or "The model did not return a usable verdict.",
            "recommended_action": "Review the anomaly manually in SkyFall.",
            "confidence": 0.0,
        }
    sev = str(v.get("severity", "")).strip().lower()
    if sev not in _SEVERITIES:
        sev = fallback_severity
    try:
        conf = float(v.get("confidence", 0.5))
        conf = max(0.0, min(1.0, conf))
    except (TypeError, ValueError):
        conf = 0.5
    return {
        "severity": sev,
        "headline": str(v.get("headline", ""))[:140] or "No headline",
        "root_cause": str(v.get("root_cause", ""))[:400],
        "explanation": str(v.get("explanation", ""))[:500],
        "recommended_action": str(v.get("recommended_action", ""))[:300],
        "confidence": conf,
    }


def post_openai(base_url: str, model: str, api_key: Optional[str],
                prompt: str, timeout_s: float) -> str:
    """POST to `/v1/chat/completions`; returns the assistant's content text.

    Leaves the resident-model policy to the server (see [`with_keep_alive`] for
    the bounded version the narrator actually uses).
    """
    return _post(base_url, model, api_key, prompt, timeout_s, None)


def with_keep_alive(keep_alive_s: float) -> Transport:
    """`post_openai` bound to a model keep-alive, in seconds (`0` = unload now).

    Only Ollama can be told to release a model, and only on its native API:
    `/v1/chat/completions` silently drops `keep_alive`, leaving the runner
    resident for its own 5-minute default. So the endpoint is probed once and,
    when it is Ollama, the verdict is requested over `/api/chat` instead.
    """

    def transport(base_url: str, model: str, api_key: Optional[str],
                  prompt: str, timeout_s: float) -> str:
        return _post(base_url, model, api_key, prompt, timeout_s, keep_alive_s)

    return transport


@lru_cache(maxsize=8)
def speaks_ollama(base_url: str) -> bool:
    """True when `base_url` serves Ollama's native API (cached per endpoint).

    `/api/tags` is Ollama's model list — the same call the core makes to decide
    whether it can offer model installs. Anything else (a cloud provider, a
    llama.cpp server, a proxy) answers 404 or worse, which just means "not
    Ollama": it is not an error, and it must never be read as a missing model.
    """
    url = f"{base_url.rstrip('/')}/api/tags"
    try:
        with urllib.request.urlopen(url, timeout=ENDPOINT_PROBE_TIMEOUT_S) as resp:
            return resp.status == 200
    except Exception as exc:  # unreachable, TLS, 404, JSON-less body: all "no"
        log.debug("narrator: %s is not Ollama (%s)", base_url, exc)
        return False


def _post(base_url: str, model: str, api_key: Optional[str],
          prompt: str, timeout_s: float, keep_alive_s: Optional[float]) -> str:
    base = base_url.rstrip("/")
    if keep_alive_s is not None and speaks_ollama(base):
        return _post_ollama(base, model, prompt, timeout_s, keep_alive_s)
    return _post_openai(base, model, api_key, prompt, timeout_s, keep_alive_s)


def _keep_alive_field(keep_alive_s: float) -> str:
    """Ollama's keep-alive as a duration string; `"0s"` unloads after the reply."""
    return f"{max(0.0, float(keep_alive_s)):g}s"


def _messages(prompt: str) -> list[dict[str, str]]:
    return [
        {"role": "system", "content": SYSTEM_PROMPT},
        {"role": "user", "content": prompt},
    ]


def _read(req: urllib.request.Request, model: str, timeout_s: float,
          pick: Callable[[Any], str]) -> str:
    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            body = json.loads(resp.read())
    except urllib.error.HTTPError as exc:
        # 404 almost always means "model not pulled" on Ollama. That is a fixable
        # setup gap, not a dead endpoint, so it must not count toward the
        # circuit breaker that disables narration for the session.
        if exc.code == 404:
            raise ModelNotInstalled(model) from exc
        raise
    return pick(body)


def _post_openai(base: str, model: str, api_key: Optional[str], prompt: str,
                 timeout_s: float, keep_alive_s: Optional[float]) -> str:
    endpoint = f"{base}/chat/completions" if base.endswith("/v1") \
        else f"{base}/v1/chat/completions"
    payload: dict[str, Any] = {
        "model": model,
        "messages": _messages(prompt),
        "temperature": 0.2,
        "max_tokens": MAX_TOKENS,
        "stream": False,
    }
    headers = {"Content-Type": "application/json"}
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"
    req = urllib.request.Request(
        endpoint, data=json.dumps(payload).encode(), headers=headers, method="POST")
    return _read(req, model, timeout_s,
                 lambda b: b["choices"][0]["message"]["content"])


def _post_ollama(base: str, model: str, prompt: str, timeout_s: float,
                 keep_alive_s: float) -> str:
    payload = {
        "model": model,
        "messages": _messages(prompt),
        "stream": False,
        "keep_alive": _keep_alive_field(keep_alive_s),
        "options": {"temperature": 0.2, "num_predict": MAX_TOKENS},
    }
    req = urllib.request.Request(
        f"{base}/api/chat", data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    return _read(req, model, timeout_s, lambda b: b["message"]["content"])


class Narrator:
    def __init__(self, cfg: Optional[dict[str, Any]] = None,
                 transport: Optional[Transport] = None):
        cfg = cfg or {}
        self.enabled = bool(cfg.get("enabled", True))
        self.base_url = str(cfg.get("base_url", "http://localhost:11434"))
        self.model = str(cfg.get("model") or DEFAULT_MODEL)
        self.api_key = cfg.get("api_key") or None
        self.timeout_s = float(cfg.get("timeout_s") or DEFAULT_TIMEOUT_S)
        self.throttle_s = float(cfg.get("throttle_s", 300.0))
        self.keep_alive_s = float(cfg.get("keep_alive_s", DEFAULT_KEEP_ALIVE_S))
        self.max_per_burst = max(0, int(cfg.get("max_per_burst",
                                                DEFAULT_MAX_PER_BURST)))
        self._transport = transport or with_keep_alive(self.keep_alive_s)
        # Only meaningful when the caller has not supplied its own transport, and
        # only while `keep_alive_s` is 0: a user who asked for a resident model has
        # already paid for it, so their value wins for every call.
        self._burst_transport: Optional[Transport] = None
        if transport is None and self.keep_alive_s <= 0:
            self._burst_transport = with_keep_alive(DEFAULT_BURST_KEEP_ALIVE_S)
        self._last: dict[tuple[str, str], float] = {}
        self._fingerprints: dict[str, float] = {}
        self._failures = 0
        self._available = True
        self._model_missing = False
        self._missing_since = 0.0

    def status(self) -> str:
        if not self.enabled:
            return "off (narrator disabled)"
        if self._model_missing:
            return f"off ({self.model} not installed - install it in Settings)"
        if not self._available:
            return "off (endpoint unreachable this session)"
        return f"on ({self.model} @ {self.base_url})"

    def narrate(self, events: list[AnomalyEvent],
                now_sec: Optional[float] = None) -> list[dict[str, Any]]:
        """Return wire-ready `narrative` messages for events worth narrating.

        A batch is one snapshot's alerts, and the throttle keys on `(kind, scope)`
        — that is per *sensor*, so five temperature probes tripping together are
        five keys. Narrating each one is minutes of full-core inference for a
        handful of overlapping stories, so: collapse alerts carrying identical
        evidence, then spend at most `max_per_burst` verdicts on the worst of what
        is left. Everything dropped is still stored and shown in the feed with its
        detector verdict, and stays eligible again once `throttle_s` passes.
        """
        if not self.enabled or not self._available or not events:
            return []
        out: list[dict[str, Any]] = []
        t = now_sec if now_sec is not None else time.time()
        # When the model is missing we pause rather than 404 on every event, but we
        # re-probe periodically so a model installed from Settings is picked up
        # without restarting SkyFall.
        if self._model_missing and (t - self._missing_since) < MODEL_RECHECK_S:
            return []
        candidates = self._candidates(events, t)
        if not candidates:
            return []
        planned = candidates[: self.max_per_burst or len(candidates)]
        if len(planned) < len(candidates):
            log.info(
                "narration burst: %d of %d alerts narrated (max_per_burst=%d); "
                "the rest keep their detector verdict and are re-offered after "
                "the %gs throttle",
                len(planned), len(candidates), self.max_per_burst, self.throttle_s,
            )
        for i, (ev, fingerprint, also) in enumerate(planned):
            key = (ev.kind, ev.scope)
            try:
                content = self._transport_for(i == len(planned) - 1)(
                    self.base_url, self.model, self.api_key,
                    self._prompt(ev, also), self.timeout_s,
                )
            except ModelNotInstalled as exc:
                # Setup gap, not an outage: keep the narrator armed (so it
                # recovers the moment the model is installed) but stop calling.
                if not self._model_missing:
                    log.warning(
                        "model %s is not installed at %s - install it from "
                        "Settings > AI insights; narration is paused (not disabled)",
                        exc, self.base_url,
                    )
                self._model_missing = True
                self._missing_since = t
                continue
            except Exception as exc:
                self._failures += 1
                log.warning("narrator call failed (%d/3): %s", self._failures, exc)
                if self._failures >= 3:
                    self._available = False
                    log.warning("narrator disabled for this session")
                continue
            self._failures = 0
            self._model_missing = False
            self._last[key] = t
            self._remember(fingerprint, t)
            verdict = parse_verdict(content, ev.severity)
            out.append({
                "type": "narrative",
                "ts_ms": int(t * 1000),
                "alert_ts_ms": ev.ts_ms,
                "alert_kind": ev.kind,
                "alert_scope": ev.scope,
                "alert_message": ev.message,
                "verdict": verdict,
            })
        return out

    def _transport_for(self, is_last: bool) -> Transport:
        """The transport for one call: warm mid-burst, unloading on the last one.

        Warmth is what makes a burst cheap (no reload, cached prompt prefix), and
        unloading on the final call is what keeps the RAM promise: a burst that
        ends leaves nothing resident, exactly like a single alert.
        """
        if is_last or self._burst_transport is None:
            return self._transport
        return self._burst_transport

    def _candidates(self, events: list[AnomalyEvent], t: float
                    ) -> list[tuple[AnomalyEvent, str, list[str]]]:
        """`(event, fingerprint, also_affected)` per story worth narrating, worst first.

        Two filters run here. The throttle drops repeats of one `(kind, scope)`
        inside `throttle_s`. The fingerprint drops alerts that would ask the model
        the same question twice: same detector, metric and severity, same
        machine-wide state line, only a different sensor. Those are *grouped*
        rather than dropped, so the surviving verdict can name what it covered.
        """
        out: list[tuple[AnomalyEvent, str, list[str]]] = []
        groups: dict[str, int] = {}
        for ev in events:
            if t - self._last.get((ev.kind, ev.scope), -1e18) < self.throttle_s:
                continue
            fingerprint = _fingerprint(ev)
            if t - self._fingerprints.get(fingerprint, -1e18) < self.throttle_s:
                continue
            at = groups.get(fingerprint)
            if at is not None:
                kept, fp, also = out[at]
                if _rank(ev) < _rank(kept):
                    # The newcomer reads worse, so it becomes the narrated sensor
                    # and the demoted one joins the "also affected" list: among 32
                    # hot cores the first to trip is not the hottest.
                    out[at] = (ev, fp, also + [kept.scope])
                else:
                    also.append(ev.scope)
                continue
            groups[fingerprint] = len(out)
            out.append((ev, fingerprint, []))
        out.sort(key=lambda c: _rank(c[0]))
        return out

    def _remember(self, fingerprint: str, t: float) -> None:
        """Record a narrated story so its twins stay quiet for `throttle_s`."""
        self._fingerprints.pop(fingerprint, None)  # re-insert: newest last
        self._fingerprints[fingerprint] = t
        while len(self._fingerprints) > FINGERPRINT_MEMORY:
            del self._fingerprints[next(iter(self._fingerprints))]

    @staticmethod
    def _prompt(ev: AnomalyEvent, also: Optional[list[str]] = None) -> str:
        lines = [
            f"Anomaly kind={ev.kind} on metric '{ev.metric}' (scope '{ev.scope}') "
            f"detector severity '{ev.severity}'.",
            f"Message: {ev.message}",
        ]
        dossier = ev.dossier or "(none provided)"
        if "\n" in dossier:
            state, causal = dossier.split("\n", 1)
            lines.append(f"Current state: {state}")
            lines.append(
                "Causal context (history-derived — prefer this for root_cause):\n" + causal
            )
        else:
            lines.append(f"Current state: {dossier}")
        if also:
            # Sibling sensors collapsed into this verdict; say so rather than
            # letting the explanation imply a single sensor was affected.
            lines.append(
                f"The same conditions also affected: {', '.join(also)}. "
                "Cover them in the explanation."
            )
        lines.append(
            "Give the single most likely root cause, then explain it, then one action. "
            "If the causal context names resource hogs or co-moving metrics, attribute the "
            "cause to them."
        )
        return "\n".join(lines)

class NarrationWorker:
    """Narrate on a background thread so a slow LLM cannot stall detection.

    One verdict can take ~90s on a CPU-only machine, and `narrate()` makes one call
    per event. Run inline, that blocks the sidecar's stdin loop for the whole
    verdict: the core keeps writing snapshots, the 64KB pipe fills after roughly
    ten, and `Sidecar::feed` then blocks, which freezes sampling, history writes
    and the dashboard alike. Monitoring is the product, so narration is moved off
    the read path entirely: events are queued, one worker drains them, and a burst
    is absorbed by the queue instead of by the collector.
    """

    def __init__(self, narrator: Narrator, emit_fn: Callable[[dict[str, Any]], None],
                 maxsize: int = 32) -> None:
        self._narrator = narrator
        self._emit = emit_fn
        self._q: queue.Queue[list[AnomalyEvent]] = queue.Queue(maxsize=maxsize)
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._warned_drop = False

    def start(self) -> None:
        """Start the worker thread (idempotent)."""
        if self._thread is not None:
            return
        self._thread = threading.Thread(
            target=self._run, name="skyfall-narrator", daemon=True)
        self._thread.start()

    def submit(self, events: list[AnomalyEvent]) -> None:
        """Queue events for narration without ever blocking the caller.

        On overflow the *oldest* pending batch is dropped: a stale verdict is worth
        less than a fresh one, and the narrator's own throttle already discards
        repeats of the same (kind, scope).
        """
        if not events:
            return
        try:
            self._q.put_nowait(events)
            return
        except queue.Full:
            pass
        try:
            self._q.get_nowait()
            self._q.put_nowait(events)
        except (queue.Empty, queue.Full):  # pragma: no cover - racing worker
            return
        if not self._warned_drop:
            self._warned_drop = True
            log.warning("narration queue full; dropping oldest pending alerts")

    def _run(self) -> None:
        while not self._stop.is_set() or not self._q.empty():
            try:
                batch = self._q.get(timeout=0.2)
            except queue.Empty:
                continue
            try:
                for narrative in self._narrator.narrate(batch):
                    self._emit(narrative)
            except Exception as exc:  # pragma: no cover - resilience net
                log.warning("narrator worker error: %s", exc)

    def stop(self, drain_timeout: float = 2.0) -> None:
        """Signal shutdown and wait briefly for in-flight narration.

        The wait is deliberately short: a verdict that has not landed by quit time
        is not worth delaying the user's exit, and the alert itself is already
        stored and shown without it.
        """
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=max(0.0, drain_timeout))
