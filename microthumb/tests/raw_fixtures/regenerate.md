# How `pins.rs` was built

The samples come from [raw.pixls.us](https://raw.pixls.us), the CC0 sample
library maintained for darktable/RawTherapee. It replaced rawsamples.ch, which
was CC BY-NC-SA.

Regenerating the table is a curation job, not something to automate into the
test run — the whole point of the pin table is that adding a sample is a
deliberate, reviewed act. The procedure that produced the current table:

1. **Fetch the index.** The site's file list is DataTables-driven, so it is not
   in the page HTML; it comes from `https://raw.pixls.us/json/getrepository.php?set=all`
   (2016 rows at the time of writing). Each row carries make, model, variant,
   size, a licence link, a download link and a published sha256.

2. **Filter to CC0.** Exactly two licence strings appear across the library:
   `publicdomain/zero` (CC0 1.0) and `licenses/by-nc-sa/4.0`. 146 rows are
   CC BY-NC-SA and are excluded. That the field discriminates is what makes it
   usable as provenance — it is not a constant.

3. **Select.** Ten per format, smallest first, one per camera model. Smallest
   first keeps the cache small and is harmless here: container structure, not
   pixel count, is what these tests exercise. One per model buys vendor and
   generation spread, which is what actually varies the IFD layout.

4. **Check size before committing to the picks.** `Content-Length` on each
   candidate, summed, against a ~2 GB ceiling. The current set is
   1,046,599,886 bytes (0.97 GiB) for 100 files, so no format had to be
   trimmed.

5. **Download once and verify against the published sha256.** A supply-chain
   double-check: the pinned BLAKE3 is computed from bytes that already matched
   the hash the library publishes independently.

6. **Sniff the container magic and compare it to the extension.** This is
   worth doing rather than trusting the file name. Two candidates were dropped
   because the two disagreed:
   - `Canon - PowerShot S2 IS - 10bit 10bit DNG, CHDK ver. 1.0.0-1504 (4:3).CR2`
     — a CHDK-produced DNG wearing a `.CR2` extension.
   - `Samsung - SM-G973U - 16bit 16bit (2.1132075471698).dng` — a plain JPEG
     (`FFD8FFE0 ... JFIF`) wearing a `.dng` extension.

   Both would have quietly mischaracterised their format. The `container`
   column records what the bytes actually say.

7. **Emit the table**, sorted by format then size.

## Adding a sample

Pin the exact URL, the BLAKE3 of the bytes you verified, the exact length, and
the container magic you observed. Confirm the licence on the library's own row
for that file. `pins_are_well_formed` and `all_fixtures_are_cc0` run without
needing the bytes and will catch the mechanical mistakes.

## Cache location

`../.fixture-cache/raw` relative to the crate, i.e. the workspace root —
outside `target/`, so `cargo clean` does not cost a 1 GiB re-download.
Gitignored; the samples are never committed. Override with
`MICROTHUMB_RAW_FIXTURE_DIR`. `MICROTHUMB_RAW_FIXTURES=offline` uses only what
is already cached.
