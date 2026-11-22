// EmbArch's report helper (embarch-api decision 109). The one package a report
// imports besides lilaq, and the only way a report draws a plot.
//
// `plot-ticks` is the algorithm of embarch-study-designer's hil-pdf.md, linear
// axes only, written to do the same operations as embarch-api's Rust
// `plot_ticks` (a test holds the two to each other): f64 arithmetic with
// compare-corrections, and tick texts by integer arithmetic, never by
// formatting a float.

#import "@preview/lilaq:0.6.0" as lq

#let tick-target = 5

#let _pow10(e) = if e >= 0 { calc.pow(10.0, e) } else { 1.0 / calc.pow(10.0, -e) }
#let _step(m, e) = if e >= 0 { m * calc.pow(10.0, e) } else { m / calc.pow(10.0, -e) }

// n × 10^e as text with `decimals` digits after the point.
#let _tick-text(n, e, decimals) = {
  let a = calc.abs(n)
  if e >= 0 {
    let v = a * calc.pow(10, e)
    return if v == 0 { "0" } else { (if n < 0 { "-" } else { "" }) + str(v) }
  }
  let scale = calc.pow(10, decimals)
  let ip = str(calc.quo(a, scale))
  let fp = str(calc.rem(a, scale))
  fp = "0" * (decimals - fp.len()) + fp
  (if n < 0 and a != 0 { "-" } else { "" }) + ip + "." + fp
}

// -> (texts, positions, lo, hi): the tick texts, their positions, and the axis
// limits first_k × step and last_k × step.
#let plot-ticks(lo, hi) = {
  assert(lo <= hi, message: "plot-ticks: lo > hi")
  if lo == hi {
    let t = str(lo).replace("\u{2212}", "-")
    return (texts: (t,), positions: (lo,), lo: lo - 1.0, hi: lo + 1.0)
  }
  let raw = (hi - lo) / (tick-target - 1)
  let e = calc.floor(calc.log(raw, base: 10))
  while _pow10(e) > raw { e -= 1 }
  while _pow10(e + 1) <= raw { e += 1 }
  let f = raw / _pow10(e)
  let m = if f < 1.5 { 1 } else if f < 3.0 { 2 } else if f < 7.0 { 5 } else { 10 }
  if m == 10 { m = 1; e += 1 }
  let step = _step(m, e)
  let first = calc.floor(lo / step)
  while first * step > lo { first -= 1 }
  while (first + 1) * step <= lo { first += 1 }
  let last = calc.ceil(hi / step)
  while last * step < hi { last += 1 }
  while (last - 1) * step >= hi { last -= 1 }
  let decimals = calc.max(0, -e)
  let ks = range(first, last + 1)
  (
    texts: ks.map(k => _tick-text(k * m, e, decimals)),
    positions: ks.map(k => k * step),
    lo: first * step,
    hi: last * step,
  )
}

// A path inside a package resolves in the package, never in the project, so a
// report hands the helper `path("capture.csv")`, a path resolved where it was
// written (Typst's `path` type), and the helper reads through that.
#let _need-path(p, what) = assert(
  type(p) == std.path,
  message: what + " must be a path: write path(\"file\") in the report",
)

// A plot of one column against another from a staged CSV capture, drawn as the
// per-bin minimum and maximum of `y` over `bins` runs of consecutive rows,
// never an average. The raw file in the zip holds the detail.
#let minmax-plot(
  file,
  x: "x",
  y: "y",
  bins: 200,
  width: 12cm,
  height: 6cm,
  xlabel: none,
  ylabel: none,
) = {
  _need-path(file, "minmax-plot's file")
  let rows = csv(file, row-type: dictionary)
  let n = rows.len()
  assert(n > 0, message: "minmax-plot: the capture has no rows")
  let nb = calc.min(bins, n)
  let xs = ()
  let mins = ()
  let maxs = ()
  for b in range(nb) {
    let seg = rows.slice(calc.quo(b * n, nb), calc.quo((b + 1) * n, nb))
    let ys = seg.map(r => float(r.at(y)))
    xs.push(float(seg.first().at(x)))
    mins.push(calc.min(..ys))
    maxs.push(calc.max(..ys))
  }
  let xt = plot-ticks(calc.min(..xs), calc.max(..xs))
  let yt = plot-ticks(calc.min(..mins), calc.max(..maxs))
  lq.diagram(
    width: width,
    height: height,
    xlabel: xlabel,
    ylabel: ylabel,
    xlim: (xt.lo, xt.hi),
    ylim: (yt.lo, yt.hi),
    xaxis: (ticks: xt.positions.zip(xt.texts), subticks: none),
    yaxis: (ticks: yt.positions.zip(yt.texts), subticks: none),
    lq.fill-between(xs, mins, y2: maxs, stroke: none),
    lq.plot(xs, mins, mark: none),
    lq.plot(xs, maxs, mark: none),
  )
}

// A measurement as the study named it, with its unit (a pure number has none).
#let m(name, file) = {
  _need-path(file, "m's file")
  let hit = json(file).measurements.find(r => r.name == name)
  assert(hit != none, message: "m: no measurement named " + name)
  if hit.unit == "1" { [#hit.value] } else { [#hit.value~#hit.unit] }
}
