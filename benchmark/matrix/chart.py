"""Self-contained HTML chart renderer (inline CSS/JS, no external assets).

Visual language follows the reth-2.0 announcement chart: neutral grey bars for
the baseline/legacy configs, green accent for the last (new) config, with
"% lower / % higher" captions vs the baseline.

Color notes (validated with the dataviz palette validator): the greys are
deliberate de-emphasis neutrals, not categorical hues, so identity never rides
on color alone — every bar carries a direct config label, a legend is present,
and a full table view sits under the chart. CVD separation between the grey
steps and the accent is ΔE 22 (target >= 12). The light grey is sub-3:1 on the
light surface, which the always-visible labels and table satisfy (relief rule).
"""

from __future__ import annotations

import html
import json
from pathlib import Path

from .analysis import METRICS, delta_caption, is_improvement

# Bar fills: greys for legacy/baseline configs (lightest = baseline), accent
# green for the last config. Same values pass contrast on both surfaces.
GREY_STEPS = ["#c3c2b7", "#898781", "#5a5955"]
ACCENT = "#008300"


def _config_colors(config_names: list[str]) -> dict[str, str]:
    colors = {}
    for i, name in enumerate(config_names):
        if i == len(config_names) - 1 and len(config_names) > 1:
            colors[name] = ACCENT
        else:
            colors[name] = GREY_STEPS[min(i, len(GREY_STEPS) - 1)]
    return colors


def _fmt_value(metric_key: str, value: float) -> str:
    if metric_key.endswith("_ms"):
        return f"{value:,.2f} ms"
    return f"{value:.2f} Ggas/s"


def render_chart_html(summary: dict, title: str = "reth-bsc storage benchmark") -> str:
    config_names = [c["name"] for c in summary["groups"][0]["configs"]] if summary["groups"] else []
    colors = _config_colors(config_names)
    baseline = summary["baseline"]

    legend = "".join(
        f'<span class="key"><span class="swatch" style="background:{colors[n]}"></span>'
        f"{html.escape(n)}</span>"
        for n in config_names
    )

    panels = []
    for group in summary["groups"]:
        sections = []
        for key, label, _lower_better in METRICS:
            valid = [c for c in group["configs"] if c["valid"]]
            max_val = max((c[key] for c in valid), default=0) or 1
            rows = []
            for c in group["configs"]:
                name = html.escape(c["name"])
                if not c["valid"]:
                    rows.append(
                        f'<div class="bar-row"><span class="cfg">{name}</span>'
                        f'<span class="invalid">invalid run</span></div>'
                    )
                    continue
                pct_w = c[key] / max_val * 100
                value = _fmt_value(key, c[key])
                delta_html = ""
                if c["deltas"] is not None:
                    pct = c["deltas"][key]
                    cls = "good" if is_improvement(key, pct) else "bad"
                    delta_html = f'<span class="delta {cls}">{delta_caption(key, pct)}</span>'
                tip = html.escape(
                    json.dumps(
                        {
                            "config": c["name"],
                            "metric": label,
                            "value": value,
                            "blocks": c["n_blocks"],
                            "total_gas": f"{c['total_gas']:,}",
                        }
                    ),
                    quote=True,
                )
                rows.append(
                    f'<div class="bar-row" data-tip="{tip}">'
                    f'<span class="cfg">{name}</span>'
                    f'<span class="track"><span class="bar" '
                    f'style="width:{pct_w:.2f}%;background:{colors[c["name"]]}"></span></span>'
                    f'<span class="val">{html.escape(value)}</span>{delta_html}'
                    f"</div>"
                )
            sections.append(
                f'<div class="metric"><div class="metric-name">{html.escape(label)}</div>'
                f'<div class="bars">{"".join(rows)}</div></div>'
            )
        n_blocks = next((c["n_blocks"] for c in group["configs"] if c["valid"]), None)
        sub = f"{n_blocks} blocks measured" if n_blocks is not None else "no valid runs"
        panels.append(
            f'<section class="panel"><h2>{html.escape(group["label"])}</h2>'
            f'<div class="sub">{sub}</div>{"".join(sections)}</section>'
        )

    table_rows = []
    for group in summary["groups"]:
        for c in group["configs"]:
            gname = html.escape(group["label"])
            name = html.escape(c["name"]) + (" (baseline)" if c["name"] == baseline else "")
            if not c["valid"]:
                table_rows.append(
                    f"<tr><td>{gname}</td><td>{name}</td>"
                    f'<td colspan="5" class="invalid">invalid run</td></tr>'
                )
                continue
            table_rows.append(
                f"<tr><td>{gname}</td><td>{name}</td>"
                f'<td class="num">{c["p50_ms"]:,.2f}</td>'
                f'<td class="num">{c["p90_ms"]:,.2f}</td>'
                f'<td class="num">{c["throughput_ggas_s"]:.4f}</td>'
                f'<td class="num">{c["n_blocks"]}</td>'
                f'<td class="num">{c["total_gas"]:,}</td></tr>'
            )

    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{html.escape(title)}</title>
<style>
:root {{
  --surface: #fcfcfb; --page: #f9f9f7;
  --ink: #0b0b0b; --ink-2: #52514e; --muted: #898781;
  --hairline: #e1e0d9; --good: #006300; --bad: #d03b3b;
}}
@media (prefers-color-scheme: dark) {{
  :root {{
    --surface: #1a1a19; --page: #0d0d0d;
    --ink: #ffffff; --ink-2: #c3c2b7; --muted: #898781;
    --hairline: #2c2c2a; --good: #0ca30c; --bad: #d03b3b;
  }}
}}
* {{ box-sizing: border-box; margin: 0; }}
body {{
  font: 14px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif;
  background: var(--page); color: var(--ink); padding: 32px 24px 64px;
}}
.wrap {{ max-width: 1280px; margin: 0 auto; background: var(--surface);
  border: 1px solid var(--hairline); border-radius: 8px; padding: 32px 36px; }}
h1 {{ font-size: 22px; font-weight: 600; }}
.legend {{ margin: 8px 0 24px; display: flex; gap: 18px; flex-wrap: wrap;
  color: var(--ink-2); font-size: 13px; }}
.key {{ display: inline-flex; align-items: center; gap: 6px; }}
.swatch {{ width: 12px; height: 12px; border-radius: 3px; display: inline-block; }}
.panels {{ display: flex; gap: 48px; flex-wrap: wrap; }}
.panel {{ flex: 1 1 460px; min-width: 380px; }}
.panel h2 {{ font-size: 17px; font-weight: 600; border-top: 1px solid var(--hairline);
  padding-top: 16px; }}
.sub {{ color: var(--muted); font-size: 12.5px; margin-bottom: 10px; }}
.metric {{ display: flex; gap: 16px; padding: 14px 0; border-bottom: 1px solid var(--hairline); }}
.metric-name {{ flex: 0 0 92px; color: var(--ink-2); font-weight: 500; padding-top: 2px; }}
.bars {{ flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 7px; }}
.bar-row {{ display: flex; align-items: center; gap: 8px; }}
.cfg {{ flex: 0 0 96px; text-align: right; color: var(--muted); font-size: 12px;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }}
.track {{ flex: 0 1 46%; min-width: 60px; }}
.bar {{ display: block; height: 16px; border-radius: 0 4px 4px 0; min-width: 2px; }}
.val {{ color: var(--ink); font-size: 12.5px; white-space: nowrap; }}
.delta {{ font-size: 11.5px; white-space: nowrap; }}
.delta.good {{ color: var(--good); }}
.delta.bad {{ color: var(--bad); }}
.invalid {{ color: var(--bad); font-size: 12.5px; }}
table {{ border-collapse: collapse; margin-top: 36px; width: 100%; font-size: 13px; }}
caption {{ text-align: left; color: var(--ink-2); font-weight: 600; padding-bottom: 8px; }}
th, td {{ padding: 6px 12px; border-bottom: 1px solid var(--hairline); text-align: left; }}
th {{ color: var(--muted); font-weight: 500; }}
td.num, th.num {{ text-align: right; font-variant-numeric: tabular-nums; }}
.overflow {{ overflow-x: auto; }}
#tip {{ position: fixed; pointer-events: none; background: var(--surface);
  border: 1px solid var(--hairline); border-radius: 6px; padding: 8px 10px;
  font-size: 12px; color: var(--ink); box-shadow: 0 2px 10px rgba(0,0,0,.15);
  display: none; z-index: 10; max-width: 280px; }}
#tip .t-metric {{ color: var(--muted); }}
</style>
</head>
<body>
<div class="wrap">
  <h1>{html.escape(title)}</h1>
  <div class="legend">{legend}</div>
  <div class="panels">{"".join(panels)}</div>
  <div class="overflow">
  <table>
    <caption>All results (deltas in the chart are vs {html.escape(baseline)})</caption>
    <thead><tr><th>Group</th><th>Config</th><th class="num">P50 (ms)</th>
      <th class="num">P90 (ms)</th><th class="num">Ggas/s</th>
      <th class="num">Blocks</th><th class="num">Total gas</th></tr></thead>
    <tbody>{"".join(table_rows)}</tbody>
  </table>
  </div>
</div>
<div id="tip"></div>
<script>
const tip = document.getElementById('tip');
document.querySelectorAll('.bar-row[data-tip]').forEach(row => {{
  const d = JSON.parse(row.dataset.tip);
  row.addEventListener('mousemove', e => {{
    tip.innerHTML = '<div><strong>' + d.config + '</strong> <span class="t-metric">' +
      d.metric + '</span></div><div>' + d.value + '</div>' +
      '<div class="t-metric">' + d.blocks + ' blocks · ' + d.total_gas + ' gas</div>';
    tip.style.display = 'block';
    const x = Math.min(e.clientX + 14, window.innerWidth - tip.offsetWidth - 8);
    const y = Math.min(e.clientY + 14, window.innerHeight - tip.offsetHeight - 8);
    tip.style.left = x + 'px'; tip.style.top = y + 'px';
  }});
  row.addEventListener('mouseleave', () => {{ tip.style.display = 'none'; }});
}});
</script>
</body>
</html>
"""


def write_chart(summary: dict, path: str | Path, title: str = "reth-bsc storage benchmark") -> None:
    Path(path).write_text(render_chart_html(summary, title))
