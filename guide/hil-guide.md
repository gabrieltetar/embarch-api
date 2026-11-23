# The HIL guide

**Status:** active, 2026-11-23. HIL milestone 1, unit M1.13 (EMB-228): what every mode shares, and the Study. Live and Test are not written yet. Written for the agent first, readable by an engineer. `embarch-api` embeds a pinned copy of this file and serves it: `embarch-api guide hil` prints "Every mode", and `embarch-api guide hil study` prints "Study". The design behind it is `embarch-hil-proposal.md` and `notes/hil/` in `embarch-doc`.

## Every mode

**Interactive sessions only.** A fleet or background agent never touches hardware, and nothing in this guide is a fleet task.

### What you are trusted with

**The claim, in two sentences, which the design is held to:**

- **No number, verdict or identity in an attested published report depends on the agent having told the truth; every sentence the agent wrote is labelled as its own.** Its exceptions, each shown in the report: a number in prose that cites no measurement is listed, not refused; a tag (`(derived)`, `(spec)`, `(inference)`, `(analysis)`) is your choice, shown as such; a value resting on a declaration you made on an unattested Core reads "declared by the agent"; even attested, a Study's own declarations (a `dut` value's resolution and `U`, a declared sample rate) are your word, labelled "declared by the study, unconfirmed", a `dut`-marked value is the word of firmware you may have written, and the build's commit, dirty state and toolchain are the build's claims; and on an unattested Core you can read the signing key, so an unattested zip's signatures prove its integrity only, and `verify` says so.
- **You do to the hardware only what the engineer allowed: a study or a test written in a file the engineer can read before it runs, and shown in a preview before a Test starts, or a Live action on the free list; every call to Core is logged.** Hardware safety is an authoring concern, not a runtime limit: nothing enforces an envelope while a study runs. That is weaker than an enforced limit, by the owner's choice.

**You are outside the evidence chain.** The chain is stimulus, measurement, record, verdict, report, and each link is produced by versioned code (Core, `embarch-api`, the bench firmware) or attested by a named person. You plan, author, operate and interpret; you produce no link. The test: **the published zip would be byte for byte the same if a person had made the same tool calls**, apart from the prose labelled as yours. **The run's files are the numeric oracle**: every number you write is a measurement you cite, or it carries a tag.

| You may | You never |
|---|---|
| **Plan**: pick the mode, propose a run and show it before it starts | Decide a verdict |
| **Author**: a study and the measurements it declares, a `report.typ`, prose; each a file a person can read | Widen what the engineer declared: a limit, an input, a mapping |
| **Operate**: tool calls, inside the lease | Answer an operator prompt, or write the operator's notes, on your own |
| **Interpret**: prose, labelled as yours | Choose which runs the reader sees |
| **Diagnose**: propose causes and fixes; a fix is a new image and a new run | Edit a study or a test so that a run passes |

**Some rules rest on procedure alone**: this guide tells you, and nothing checks. An engineer reading a report should know exactly which ones:

1. **Never edit a study, a test, a limit or a plan to make a run pass.** Every run stays in Core's run log, but the edit itself is caught by nobody.
2. **An operator prompt's answer is the person's own words, never yours.** Core is designed to refuse an observation's or a clearance's answer that comes with your token; that is not built yet, so today every answer rests on this rule.
3. **Conclude from `measurements.json` and stream summaries, never from a raw capture**, which is too large for your context: a conclusion from the 200 lines you read is a conclusion from a sample.
4. **No hardware from a fleet or background agent**, ever.

**Attested and unattested.** A run is attested when its records are signed by a Core running under its own account and installed from a signed release; everything else is unattested, and every report from it says so on its first page. **Today every Core is unattested**: the attested install is a later milestone. Unattested, the person's token and yours both exist, but you can read the person's, so the scopes separate surfaces and not people, and the report says "person's acts not protected". **The usual path is development runs, then a run for record**: build and validate a study on an unattested Core, then, once the engineer is happy with it, run it for record on the attested one.

**What the engineer's container statement excludes is not settled yet.** Until the standalone pass settles your clone's place, assume you can reach every file the engineer's account can, the Core token files included, and say so if asked.

### Words

These words mean exactly this here. A firmware's unit tests are never called tests without "unit", and the hardware test ledger's rows are "ledger tests".

- **Bench**: zero or one DUT, zero or one dev bench, and any number of **bench boards** (the dev bench, a USB PD partner) and **instruments** (a load, a supply). A **saved bench** is its description in the firmware repo's `embarch/topologies/`; a **live bench** is that description bound to real probes and ports.
- **Study** (the mode): a custom question answered by measurement on exactly one bench. **A study** (the file) is the steps a runner executes: `run_study` on the dev bench, or `run_host_study` on Core's own runner.
- **Test** (the mode): technical conformance with a verdict per test, on one or more benches. A **test suite** holds **tests**; a **test plan** is a saved selection from a suite.
- **Run**: one execution of a study or a Test selection, with an id in Core's run log. A **resolved run** (`run.json`) is a Test selection with every input substituted, hashed.
- **Measurement**: a named value with a unit, a resolution, an uncertainty, who measured it (`bench`, `dut` or `person`) and the file and rows it came from, computed by Core.
- **Verdict**: pass, fail, error, inconclusive, skipped or cancelled, decided by code. A Study has none.
- **Publish**: turning a finished run into one zip holding a PDF report and every file behind it. **`verify`** checks that zip offline.

### Choosing a mode

| The request | Mode |
|---|---|
| Names a specification or conformance: "does the PD contract work", "check R3.2" | **Test** |
| A custom question whose answer is a measurement: "what rate does it sustain", "how long does wake-up take" | **Study** |
| Look or poke while developing: "what is in this register", "reset it and show me the log" | **Live** |
| A Live result about to be cited in a PR, a ticket or a report | Redo it as a **Study** or a **Test**, so it has a published receipt |

**A spec named is a Test, not a Study.** A spec named with no tests runs the project's saved test plan for that suite, and says so; with none, list the applicable tests grouped by bench and ask which to run. **A request that fits no row gets one question**: Study, Test or Live, each in one line, your pick first.

### Before any hardware

1. **`status`**: Core is reachable, who holds the lease, and `last_images`, the last image Core flashed per chip, with its `sha256`. An absent chip means unknown, not nothing flashed.
2. **`lease_take`, with a `purpose`** the engineer would recognise. Every other session's hardware call is then refused `409`, naming you and the purpose. Release it with `lease_release` when the work ends; never take a lease another session holds.
3. **The saved bench is live**: its boards are enrolled and bound, by the engineer in `embarch-ui`'s Make live or with the CLI.
4. **`validate`** passes for every role the work touches: the DUT, and the dev bench for a dev-bench study.

### What you never do

Each with the layer that stops it, or "procedure" where nothing does:

- **Answer an operator prompt on your own**: procedure today; Core's refusal of an observation or clearance answer with your token is designed, not built.
- **Decide a verdict**: code decides it.
- **Run hardware from the fleet**: procedure.
- **`git checkout` to reach a revision a study asks for**: `embarch-api` never runs a tree-changing `git` command; a mismatch fails the run, naming both revisions, and moving the tree is the engineer's call.
- **Read a truncated capture as complete**: `list_study_streams` says `truncated`, and a measurement over a truncated capture says so.
- **Edit a study, a test, a limit or a plan to make a run pass**: procedure.
- **Use an input you proposed before the engineer confirms it**: procedure today; a Test records each input's provenance once it lands.

### After a failure

**Diagnose and propose causes and fixes; do not retry until it passes.** A firmware fix is a new image and a new run, and the report shows both. A step on a bench board or an instrument that fails is the setup's fault, not the DUT's: say which.

## Live

**Not written yet.** Live is exploration during development: an answer now, a live log, findings written back. Say so to the engineer and stop short of inventing a procedure. The design is `notes/hil/05-live.md` in `embarch-doc`.

## Study

**A Study answers one question by measurement, on one bench, and ends in a published zip** that anyone can `verify` offline. Read "Every mode" (`embarch-api guide hil`) first. The worked example is a **BLE GATT data-acquisition study**: the dev bench connects to the DUT, subscribes to one data characteristic, captures what arrives, and the report states the count, the rate and the span of the DUT's sequence numbers. The study is below; `embarch-api` carries it as `tests/fixtures/ble_gatt_rate_study.json`, and a test there checks it, and this copy, against Core's own pre-flight rules on every build.

### 1. Write the study

A study is JSON in `embarch-study-designer`'s schema, saved in the firmware repo's `embarch/studies/<slug>/`. Start from the example, not from the type tables. **`action` is externally tagged**: one key, the variant name, its value the fields (`{"GattMonitorSelected": {"targets": [...]}}`).

```json
{
  "name": "ble-gatt-rate",
  "requires": { "dev_bench_version": "any", "firmware_version": "any" },
  "steps": [
    {
      "name": "connect",
      "action": {
        "BleConnect": {
          "role": "Central",
          "target_address": { "bytes": [196, 130, 225, 66, 177, 38], "kind": "Public" },
          "target_name": null
        }
      },
      "timeout_ms": 15000,
      "continue_on_fail": false
    },
    {
      "name": "capture",
      "action": {
        "GattMonitorSelected": {
          "targets": [
            {
              "service_uuid": [18, 52, 86, 120, 18, 52, 86, 120, 18, 52, 86, 120, 154, 188, 222, 240],
              "characteristic_uuid": [18, 52, 86, 120, 18, 52, 86, 120, 18, 52, 86, 120, 154, 188, 222, 241]
            }
          ]
        }
      },
      "timeout_ms": 60000,
      "continue_on_fail": false
    }
  ],
  "streams": [
    {
      "id": 0,
      "name": "data",
      "source": {
        "GattNotify": {
          "service_uuid": [18, 52, 86, 120, 18, 52, 86, 120, 18, 52, 86, 120, 154, 188, 222, 240],
          "characteristic_uuid": [18, 52, 86, 120, 18, 52, 86, 120, 18, 52, 86, 120, 154, 188, 222, 241]
        }
      },
      "encoding": { "Struct": { "decoder": 0 } },
      "scope": "WholeStudy"
    }
  ],
  "decoders": [
    { "name": "packet", "header": [{ "name": "seq", "type": "u32_le" }], "repeat": [] }
  ],
  "measurements": [
    { "name": "notifications", "reducer": "count", "tap": 0 },
    { "name": "rate", "reducer": "rate", "tap": 0 },
    { "name": "first_seq", "reducer": "min", "tap": 0, "field": "seq", "unit": "1" },
    { "name": "last_seq", "reducer": "max", "tap": 0, "field": "seq", "unit": "1" }
  ],
  "steps_crc": 0
}
```

- **`requires`**: the bench and DUT builds the study is for; `"any"` is a legal, explicit answer for either.
- **`steps`**: a `BleConnect` with `target_address` (six bytes, most significant first, and `Public` or `Random`) or `target_name`, never "the first one seen": on a real bench that reaches a neighbour. To find the DUT's address, run a `BleConnect` whose `target_name` nothing has, and read the census in its `fail_reason` and in `dev-bench.log`. Then a `GattMonitorSelected` naming the data characteristic, whose `timeout_ms` is the capture window.
- **`streams`**: a tap, `GattNotify` on the same characteristic, `WholeStudy` scope. A notify tap routes what a monitor step subscribed; it subscribes to nothing itself, so a tap on a characteristic no step monitors captures nothing.
- **`decoders`**: the packet's layout, when the tap is `Struct`: named fields with a width, sign and byte order, and nothing else. **Take the layout from the engineer or the firmware's own packet definition, and say which.** The example's `seq` field, a little-endian 32-bit counter, stands for whatever the DUT's packet really carries.
- **`measurements`**: what Core computes when the study ends, each from a built-in reducer over a tap: `count`, `rate`, `min`, `max`, `mean`, `percentile`, `interval`, `value_at_marker`. The example declares `notifications` (`count`), `rate` (`rate`), and `first_seq` and `last_seq` (`min` and `max` of `seq`, unit `"1"`). **No script you write computes a reported number**: a value you need that no reducer gives is a `(derived)` or `(analysis)` number in your prose, shown as yours.
- **`steps_crc`**: leave `0`. `run_study` and `run-study` reseal a study before they submit it.

**Check what it puts on the hardware before it runs**: nothing limits a study at run time. A BLE study drives no power, but a host study that switches a load or a supply is checked against the DUT's ratings when you write it, and the engineer reads it before it runs.

### 2. Put the right firmware on the boards

`requires` and the reflash choice decide what is built and flashed. `none`, the default, runs against what is on the boards now. `dut` (with `project`) or `dev-bench` builds the working tree **as it stands** and flashes it. Compare `status`'s `last_images` with your own image's `sha256` before you reflash. A version mismatch refuses the run, naming both; `--allow-version-mismatch` runs anyway and writes the override into the result.

### 3. Run it

- **MCP**: `run_study` with the study as an inline object (never a JSON string), then `study_watch` or `study_status` until it ends.
- **CLI**: `embarch-api run-study --study-file embarch/studies/<slug>/study.json`, then `embarch-api study-status <study_id>`.

The report's appendix says what Core observed of each image. Today a DUT image is the build's claim, with no readback through the probe, and a bench board's image reads `unverified`; say both where your prose rests on them.

### 4. Read the losses, then the measurements

1. **`list_study_streams <study_id>` first.** Every tap, its bytes, and whether it was **truncated**. A capture with 0 bytes was declared and produced nothing, which is a different problem from one never declared. Truncated data is short data: say so in every sentence that rests on it.
2. **Read `measurements.json`, not the captures.** Read it before you publish: MCP `study_measurements <study_id>`, or CLI `embarch-api study-measurements <study_id>`, returns the file as Core wrote it. A refusal names itself: `no-such-run`, `still-running` (wait for the run to end), `no-measurements` (none declared, not completed, or a host study) or `files-gone`. Each entry carries `value`, `unit`, `by`, `u_source` and `declared_by`; "U undeclared" and "declared by the agent" are what they say, and your prose repeats them.
3. Sample a capture with `study_stream_data --out` only to diagnose, never to conclude.

### 5. Publish

**Set the publish folder once**, in the firmware repo's `embarch/embarch.toml`; there is no default, and `publish` refuses without one:

```toml
[publish]
folder = "<an absolute path outside the repo>"
```

**Write the prose as a file per run**, `embarch/studies/<slug>/runs/<run id>.md`. **Every number cites the measurement it states, by name, at most two words after it**: `The DUT sent 2994 notifications [notifications] at 49.9 Hz [rate].` A number that matches its measurement, rounded to the digits you wrote, is cited; one that cites nothing is listed in the appendix as untagged or unbound. A number no measurement holds carries a tag, at most two words after it: `Missing: 6 (derived), from last_seq minus first_seq plus one, less notifications.` Say "declared by the agent" or "U undeclared" where the measurement does.

**Publish**:

- **MCP**: `publish` with `run_id`, `prose` as `[{name, text, author}]` (`author` is your model id), and `notes` only for the engineer's own words, verbatim.
- **CLI**: `embarch-api publish <run_id> --prose embarch/studies/<slug>/runs/<run id>.md --prose-author <model id>`.

The zip is `<date>-<slug>-<id8>.zip` and is never overwritten: **publishing the same run again writes `-r2`**, built from the earlier zip, and its appendix names the revision it supersedes. With no `report.typ`, the PDF is a generated data section (each capture and its rows, each measurement) and the audit appendix; your prose rides in the zip under `prose/`, and the appendix lists every number in it. A study's own `report.typ` puts prose and plots on the page: `#import "@embarch/report:0.1.0": minmax-plot, m`, then cite with `#m("rate")` and plot a signed series with `minmax-plot`; any other picture is refused, and `report` names the file at publish. **A non-empty `warnings` means the zip is written and is less than a complete report**: tell the engineer.

### 6. Verify it in a clean folder

`verify` loads no config and calls no Core: it reads the zip and a key. **Copy the zip into an empty folder and run it there**, so nothing but the zip is read:

```sh
embarch-api core-key                    # "public key": 64 hex digits, and its fingerprint
embarch-api verify <zip> --public-key <hex> [--json]
```

`core-key` asks the Core `status` reaches and prints the key only after checking Core's signature over a fresh nonce. **The key comes from Core, never from the zip.** Exit 0 means no check failed; `--json`'s `complete` says whether every check also ran in full. **Today every result says "origin unattested"**: the signatures prove the zip is intact, and the agent could have read the key that made them. Read each `partial` and `not checked by this version` aloud to the engineer, with what it leaves unchecked.

### 7. Read the report against the claim

Before you call the Study done, read the PDF's first page and the appendix as a stranger would: the attestation label, `by` beside each value, every untagged or unbound number, every truncation and "U undeclared". **Each one is either fixed in a new revision or said, in a line, to the engineer.** A gap in EmbArch itself (a check it should make, a fact it cannot read) is filed as an issue, not worked around.

## Test

**Not written yet.** A Test is technical conformance with a verdict per test, on one or more benches; no Test run exists in milestone 1. Say so to the engineer and stop short of inventing a procedure. The design is `notes/hil/03-test.md` in `embarch-doc`.
