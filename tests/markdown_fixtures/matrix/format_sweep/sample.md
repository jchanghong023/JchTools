# Attributions

This benchmark corpus is derived from third-party datasets. Each is credited below as required
by its license. Per-document provenance (upstream id, source revision, license) is in
`ground_truth/corpus_manifest.json`.

## lazyc/READoc

- **Citation:** READoc: A Unified Benchmark for Realistic Document Structured Extraction (arXiv:2409.05137)
- **Source:** <https://huggingface.co/datasets/lazyc/READoc>
- **License:** MIT
- **Used here:** 2217 accepted documents (per-doc).
- **Modifications:** ground truth normalized to canonical GFM (see README → How the data was modified). Derived GT is a derivative work under the upstream license.

## llamaindex/ParseBench

- **Citation:** ParseBench: A Document Parsing Benchmark for AI Agents, Zhang et al. (arXiv:2604.08538)
- **Source:** <https://huggingface.co/datasets/llamaindex/ParseBench>
- **License:** Apache-2.0
- **Used here:** 304 accepted documents (reference).
- **Modifications:** ground truth normalized to canonical GFM (see README → How the data was modified). Derived GT is a derivative work under the upstream license.

## bsmock/FinTabNet.c

- **Citation:** Smock et al., "Aligning benchmark datasets for table structure recognition", ICDAR 2023 (arXiv:2303.00716)
- **Source:** <https://huggingface.co/datasets/bsmock/FinTabNet.c>
- **License:** CDLA-Permissive-2.0
- **Used here:** 80 accepted documents (vendor).
- **Modifications:** ground truth normalized to canonical GFM (see README → How the data was modified). Derived GT is a derivative work under the upstream license.

## federalregister.gov

- **Citation:** U.S. Federal Register (OFR/GPO), federalregister.gov — U.S. Government work, public domain (17 U.S.C. §105)
- **Source:** <https://www.federalregister.gov>
- **License:** US-PD (17 U.S.C. §105)
- **Used here:** 31 accepted documents (vendor).
- **Modifications:** ground truth normalized to canonical GFM (see README → How the data was modified). Derived GT is a derivative work under the upstream license.

## WordPerfect corpus (`wordperfect/`)

Vendored WordPerfect-family test files. Per-file provenance (upstream path, revision, format/version) is in
`wordperfect/PROVENANCE.md`.

### apache/tika

- **Source:** <https://github.com/apache/tika> — `tika-parser-miscoffice-module` test documents
- **License:** Apache-2.0
- **Used here:** `wp50.wp`, `wp51.wp`, `wp6.wpd` (WordPerfect 5.0 / 5.1 / 6.x).
- **Modifications:** none to the source files. Markdown/plaintext ground truth is derived (see `wordperfect/PROVENANCE.md`).

### LibreOffice/core (writerperfect)

- **Source:** <https://github.com/LibreOffice/core> — `writerperfect/qa/unit/data` (libwpd / libwpg test data)
- **License:** MPL-2.0
- **Used here:** `wp42.wp`, `wp_mac1.wpd`, `wp_mac3.wpd`, `graphic_v1.wpg`, `cve_2015_1760_1.wpd`, `cve_2015_1760_2.wpd`, `cve_2007_1735_1.wpd`.
- **Modifications:** none to the source files. Ground truth is derived where applicable.

### OPF format corpus

- **Source:** <https://github.com/ross-spencer/opf-format-corpus> — `format-corpus/office/wordprocessing/wpd`
- **License:** CC0 (public-domain dedication; corpus README)
- **Used here:** `corel_wp6.wpd` (from `TOPOPREC.WPD`).
- **Modifications:** none to the source file. Ground truth is derived (see `wordperfect/PROVENANCE.md`).

### ImageMagick

- **Source:** <https://github.com/ImageMagick/ImageMagick> — `PerlMagick/t/input.wpg`
- **License:** ImageMagick License (an Apache-2.0 derivative; permissive, attribution-only) — <https://imagemagick.org/license/>
- **Used here:** `graphic_v2.wpg` (WordPerfect Graphics 2.0).
- **Modifications:** none to the source file.

## OpenDocument Presentation fixture (`odp/libreoffice_impress.odp`)

- **Source:** Apache Tika test corpus.
- **License:** Apache-2.0.
- **Used here:** `libreoffice_impress.odp`.
- **Modifications:** none to the source file. Markdown/plaintext ground truth is derived from the presentation XML
  text nodes and title/subtitle roles.

## Apple iWork fixtures (`iwork/`)

- **Source:** existing Xberg test corpus, present since corpus commit `c55c3b48ba29634d097c84cda220929a083e4553`.
- **Original upstream/license:** not recorded in the import history; no external source is inferred here.
- **Used here:** `test.pages`, `test.key`, and `test.numbers`.
- **Modifications:** none to the source files. Pages and Keynote GT comes from their sole length-delimited UTF-8 IWA
  payloads; Numbers GT uses Apple QuickLook and the package's matching application-generated preview as independent
  visual oracles.

## HWPX fixture (`hwpx/simple.hwpx`)

- **Source:** existing Xberg test corpus, present since corpus commit `c55c3b48ba29634d097c84cda220929a083e4553`.
- **Original upstream/license:** not recorded in the import history; no external source is inferred here.
- **Used here:** `simple.hwpx`.
- **Modifications:** none to the source file. Ground truth is derived from the paragraph and text-run nodes in
  `Contents/section0.xml`.

## JSONL fixtures (`jsonl/`)

- **Source:** added directly to the Xberg test corpus in commit
  `cfd6902290a4e36e8e5cf77e8a3b93ba1a3d1720` with no external upstream recorded.
- **Used here:** `simple.jsonl` and `with_blanks.jsonl`.
- **Modifications:** none to the source files. Ground truth is the exact canonical JSON array represented by each
  newline-delimited input; blank separator lines have no semantic value.

## ndl-lab/pdmocrdataset-part1 (NDL PDM OCR Dataset)

- **Citation:** National Diet Library (Japan), "NDL古典籍OCR / PDM OCR Dataset (tosho_all)", 2022.
- **Source:** <https://github.com/ndl-lab/pdmocrdataset-part1> (data: <https://lab.ndl.go.jp/dataset/pdm_ocr_dataset/line/tosho_all_linejson.zip>)
- **License:** Public Domain Mark 1.0 (source works are copyright-expired; NDL releases openly).
- **Used here:** 5 page images (`images/ndl_meiji_vertical_01..05.jpg`) — historical Japanese printed pages (Meiji–Shōwa, 1870–1930), predominantly vertical text.
- **Modifications:** each page's line-level `text` boxes (NDL human transcription) concatenated in vertical reading order (right-to-left columns, top-to-bottom within a column) into canonical GT. Word strings are verbatim NDL annotations; no fabrication.

## naver-clova-ix/cord-v2 (CORD)

- **Citation:** Park et al., "CORD: A Consolidated Receipt Dataset for Post-OCR Parsing", NeurIPS 2019 Workshop on Document Intelligence.
- **Source:** <https://huggingface.co/datasets/naver-clova-ix/cord-v2>
- **License:** CC-BY-4.0
- **Used here:** 4 receipt images (`images/cord_receipt_01..04.jpg`) from the `test` split.
- **Modifications:** `valid_line` word `text` (human annotation) joined in reading order (lines top-to-bottom, words left-to-right) into GT. Verbatim.

## TextOCR (Meta)

- **Citation:** Singh et al., "TextOCR: Towards large-scale end-to-end reasoning for arbitrary-shaped scene text", CVPR 2021.
- **Source:** <https://textvqa.org/textocr/> (images: TextVQA / Open Images; <https://huggingface.co/datasets/facebook/textvqa>)
- **License:** CC-BY-4.0
- **Used here:** 3 scene-text images (`images/textocr_scene_01..03.jpg`); image_ids `e6c1a7b56123bbdb`, `76f940b2603a49e7`, `855d76c85603018d`.
- **Modifications:** human word `utf8_string` values grouped into lines by geometry and concatenated in reading order into GT. Verbatim (null "." markers dropped).

## ds4sd/DocLayNet (IBM)

- **Citation:** Pfitzmann et al., "DocLayNet: A Large Human-Annotated Dataset for Document-Layout Analysis", KDD 2022.
- **Source:** <https://huggingface.co/datasets/ds4sd/DocLayNet-v1.1>
- **License:** CDLA-Permissive-1.0
- **Used here:** 2 financial-report pages (`images/doclaynet_page_01..02.jpg`) from the `test` split (`NASDAQ_ATRI_2003.pdf` p24, `NYSE_MGM_2004.pdf` p49).
- **Modifications:** authoritative PDF text cells linearized in reading order (top-to-bottom, then left-to-right). No OCR and no model used; text is DocLayNet's verbatim PDF-layer cell text.

## Diagram fixtures (this repository)

- **Source:** authored here, not derived from any third-party dataset. The first four Graphviz fixtures were contributed by Parman Mohammadalizadeh (@MannXo) in xberg-io/test_documents#1.
- **License:** same as this repository.
- **Used here:** `diagrams/svg/*.svg` and `diagrams/pdf/*.pdf` plus their `diagrams/src/*.{dot,mmd,puml,fodg,html}` sources, `diagrams/manifest.json`, and `ground_truth/dot/*.dot`.
- **Rendering tools:** the source graphs are ours; the SVGs and PDFs are what each renderer emitted from them. Graphviz 15.1.1 (`dot`/`neato`) is EPL-1.0, Mermaid CLI 11.16.0 is MIT, PlantUML 1.2026.0 is GPL, LibreOffice 26.2.5.2 is MPL-2.0, librsvg 2.62.3 (`rsvg-convert`) is LGPL-2.1-or-later, qpdf 12.3.2 is Apache-2.0, and Google Chrome 151 is proprietary. None of these licenses reaches the output: rendering a document is not a derivative work of the renderer, and PlantUML says so explicitly in its FAQ. No tool binary, jar or stylesheet is redistributed here.
- **Embedded fonts:** a PDF carries subsetted glyph outlines of whatever fonts the renderer reached for, so the fixtures under `diagrams/pdf/` do redistribute font data. Each family was checked against the OS/2 `fsType` bit of the system font it came from, which is where a font states its own embedding terms: Helvetica and Liberation Sans report 0 (installable, no restriction), Times New Roman and Trebuchet MS report 8 (editable embedding). All four permit it. `scripts/build_diagram_pdfs.py` holds that allowlist and fails a build that reaches for anything outside it. That check is why `graphviz_cjk` ships as SVG only: macOS's Songti reports `fsType` 2, restricted, which forbids embedding without the owner's permission, so no CJK PDF is published until an open-licensed CJK face is available to render one.
- **Modifications:** every fixture whose producer states its own graph also ships as a `*_geometry.svg` variant with that metadata removed by `scripts/strip_svg_graph_metadata.py` — Graphviz writes the node ids and the full edge list into `<title>` elements and again into XML comments; Mermaid into `id="L_start_auth_0"`, again into `data-id`, and the edge's waypoints into `data-points`; PlantUML into `id="Read config-to-Open input"`, again into a comment, again into a `data-entity-1`/`data-entity-2` pair, and once more into a `<?plantuml-src?>` processing instruction carrying its whole deflated source; and LibreOffice by tagging each group with its shape kind, which across a whole file is the node/edge partition. A fixture that names its own answer cannot measure geometry recovery. Stripping removes no geometry: each variant renders to a byte-identical PNG. `nested_transforms.svg`, `icon_nodes.svg`, `mixed_page.svg`, `two_diagrams.svg` and the three `negative_*.svg` files are hand-written. Ground truth is the source graph restated by node label, so it is independent of any recogniser's numbering.

## xberg math corpus

- **Citation:** assembled for xberg from published documents that carry mathematics, one per notation and container the extractor supports.
- **Source:** 123 upstream projects and publishers. Each document's source URL is in `MATH_PROVENANCE.md`.
- **License:** per document. 136 are vendored under permissive terms, chiefly MIT (36), CC-BY-4.0 (25), Apache-2.0 (21), US public domain (13), BSD-3-Clause (12) and MPL-2.0 (5). Four state a dual `CC-BY OR GPL-3.0-or-later` licence and are taken under the CC-BY arm. 12 carry ShareAlike, GPL or GFDL terms and are reference only, recorded without their bytes.
- **Used here:** 136 documents vendored, 12 reference.
- **Modifications:** none. Each document is byte-identical to what its publisher serves, so a fixture measures the real file rather than a rewritten one. No ground truth is derived from them here.

## PubMed Central open access

- **Citation:** Europe PMC RESTful Web Service, Europe PMC Consortium.
- **Source:** <https://europepmc.org>
- **License:** per article, read from each file's JATS `ali:license_ref`. 498 are CC-BY-4.0 or CC0 and vendored; 214 are CC-BY-NC or CC-BY-NC-ND and reference only.
- **Used here:** 712 full-text JATS articles vendored, 214 reference.
- **Modifications:** none. Each file is byte-identical to what the service returns.

## arXiv

- **Citation:** arXiv.org e-Print archive, Cornell University.
- **Source:** <https://arxiv.org>
- **License:** per paper, read from its OAI record. 204 are CC-BY-4.0 and vendored. 993 carry the arXiv perpetual non-exclusive licence, which grants distribution to arXiv rather than to third parties, so they are reference only.
- **Used here:** 204 PDFs vendored, 993 reference.
- **Modifications:** none.

## Project Gutenberg

- **Citation:** Project Gutenberg Literary Archive Foundation.
- **Source:** <https://www.gutenberg.org>
- **License:** US public domain. The Project Gutenberg trademark and licence terms attach to the header, not to the underlying public-domain work.
- **Used here:** 500 EPUBs vendored, as prose ballast that carries no mathematics.
- **Modifications:** none.

## govdocs1

- **Citation:** Garfinkel, Farrell, Roussev and Dinolt, "Bringing science to digital forensics with standardized forensic corpora" (DFRWS 2009).
- **Source:** <https://digitalcorpora.org/corpora/file-corpora/files/>
- **License:** US government works, public domain under 17 U.S.C. 105.
- **Used here:** 400 mixed office documents vendored, from shards 000 and 001.
- **Modifications:** none. Files are taken from the published shards under their original names, prefixed with the shard.

## Wikipedia

- **Citation:** Wikipedia contributors, English Wikipedia.
- **Source:** <https://en.wikipedia.org>
- **License:** CC-BY-SA-4.0. ShareAlike is copyleft, so these are reference only.
- **Used here:** 178 mathematics articles, reference.
- **Modifications:** none.

## Synthetic scanned math pages

- **Citation:** none; generated for this corpus.
- **Source:** rendered with matplotlib mathtext, rasterized at 300 DPI, binarized, and packed as CCITT G4 PDFs with no text layer, to mirror the shape of a scanned textbook page (xberg issue #1385).
- **License:** MIT, as our own work.
- **Used here:** `pdf_scanned/synthetic_math_*.pdf`, four pages that carry display equations only as pixels. They exercise formula detection and recognition on scanned input, including a skewed scan.
- **Modifications:** not applicable.

## xberg benchmark-harness fixtures

- **Citation:** none; part of the xberg repository.
- **Source:** <https://github.com/xberg-io/xberg>, `tools/benchmark-harness/fixtures/split/memo_marketing_form.pdf`, page 1, rasterized at 300 DPI and packed as a bilevel CCITT G4 scan.
- **License:** MIT, as our own work.
- **Used here:** `pdf_scanned/memo_prose_scanned.pdf`, the negative case: a scanned page with no mathematics, which must yield no formulas.
- **Modifications:** rasterized and binarized from the vector original.

## Xberg issue #1484 two-column gutter reproduction

- **Source:** authored by Eric Evers (`@erichevers`) and attached to
  <https://github.com/xberg-io/xberg/issues/1484> as a minimal reproduction.
- **License:** MIT, as a contribution to the Xberg test corpus.
- **Used here:** `pdf/issue-1484-two-column-hanging-number-gutter.pdf`, a two-page A4 fixture whose
  first page uses hanging clause numbers and whose unnumbered second page is the control.
- **Modifications:** renamed for stable corpus addressing; document bytes are otherwise unchanged.

## Xberg issue #1542 prose-paragraph-as-table reproduction

- **Source:** authored by Eric Evers (`@erichevers`) and attached to
  <https://github.com/xberg-io/xberg/issues/1542> as a minimal reproduction.
- **License:** MIT, as a contribution to the Xberg test corpus.
- **Used here:** `pdf/issue-1542-prose-paragraph-shredded-into-table.pdf`, a single unruled A4 page
  carrying three three-line body paragraphs. Paragraphs two and three share their first two lines
  verbatim and differ only in the last line, numeric in one and alphabetic in the other, so the page
  is a self-contained A/B for the table heuristic.
- **Modifications:** renamed for stable corpus addressing; document bytes are otherwise unchanged.

## City of Sugar Land, Texas — Ordinance No. 2197

- **Citation:** City of Sugar Land, Texas, Ordinance No. 2197 (adopted 4 August 2020), a zoning change for approximately 0.7906 acres at Lake Pointe Parkway and Creek Bend Drive.
- **Source:** City of Sugar Land public records.
- **License:** US public domain. Texas municipal ordinances are edicts of government and carry no copyright.
- **Used here:** `pdf_scanned/ordinance_2197_scanned.pdf`, a 16-page office-scanner capture (Xerox AltaLink C8045, JBIG2 masks over JPEG backgrounds) with no text layer. It pairs long-form legal prose with signature blocks, a recorded survey plat, and six architectural exhibit sheets whose labels are rotated 90 degrees, so it exercises OCR reading order across mixed upright and landscape content.
- **Modifications:** none to the document. Ground truth in `ground_truth/pdf/ordinance_2197.txt` was produced with PaddleOCR and proofread page by page against 300 DPI renders. It covers the ten prose pages in full, including the handwritten adoption dates; the six drawing sheets are represented only by the printed title-block text that could be read with confidence, since their surveyor and hand-lettered CAD annotations are not legibly transcribable.

## EPUB edge-case corpus

Per-file provenance, licence evidence and the measured before and after for each file: `EPUB_EDGE_CASES.md`. The sources it draws on that are not already credited above:

### Standard Ebooks

- **Citation:** Standard Ebooks, Frankenstein by Mary Shelley.
- **Source:** <https://standardebooks.org/ebooks/mary-shelley/frankenstein>
- **License:** CC0 1.0, stated in the file's `content.opf` (`dc:rights`).
- **Used here:** one EPUB, for the subtitle and multiple-subject metadata case.
- **Modifications:** none.

### IDPF epub3-samples

- **Citation:** IDPF EPUB 3 sample documents, `childrens-literature`.
- **Source:** <https://github.com/IDPF/epub3-samples>
- **License:** the file's `package.opf` states `Public domain in the USA.`; the collection's CC-BY-SA default does not apply to it, and no other sample is used.
- **Used here:** one EPUB.
- **Modifications:** none.

### W3C epub-tests

- **Citation:** W3C EPUB 3 test suite.
- **Source:** <https://github.com/w3c/epub-tests>, packaged files served from <https://w3c.github.io/epub-tests/tests/>
- **License:** W3C Software and Document License (`LICENSE.md`).
- **Used here:** six EPUBs: navigation document in the spine, an image-only fixed-layout page, two SVG-in-spine tests, a leaking relative image link, font obfuscation.
- **Modifications:** none.

### w3c/epubcheck test resources

- **Citation:** EPUBCheck, the EPUB conformance checker, test resources.
- **Source:** <https://github.com/w3c/epubcheck>, `src/test/resources`, at commit `82b174ec319ea3e6c9d2488f84155fa4a9171fc2`.
- **License:** BSD-3-Clause (`LICENSE.md`).
- **Used here:** four tests published as unpacked directories: `ocf-url-leaking-in-opf-error`, `ocf-url-path-absolute-error`, `ocf-encryption-unknown-valid`, `ocf-obfuscation-valid`.
- **Modifications:** each directory is packaged into an EPUB container by `scripts/fetch_epub_edge_cases.py`; the member bytes are unchanged.

### futurepress/epub.js

- **Citation:** epub.js test fixtures.
- **Source:** <https://github.com/futurepress/epub.js>, `test/fixtures/alice.epub`.
- **License:** BSD (repository `license` file); the text is public domain in the USA, as the file's package document states.
- **Used here:** one EPUB, for the image-only cover page.
- **Modifications:** none.

### psiegman/epublib

- **Citation:** epublib test resources.
- **Source:** <https://github.com/psiegman/epublib>, `epublib-core/src/test/resources/testbook1.epub`.
- **License:** Apache-2.0.
- **Used here:** one EPUB, for two `dc:title` elements in EPUB 2 form.
- **Modifications:** none.

### Internet Archive

- **Citation:** Internet Archive, `americana` collection.
- **Source:** <https://archive.org/details/whyiamsocialist441besa> (1886) and <https://archive.org/details/foodresearchinst00stan> (1923).
- **License:** public domain in the United States by date of publication; the items carry no rights statement.
- **Used here:** two EPUBs generated by the Archive: one Readium LCP derivative, one whose spine items are declared `text/html`.
- **Modifications:** none.

### Synthesized EPUB edge cases

- **Citation:** none; generated for this corpus by `scripts/build_epub_edge_cases.py`.
- **License:** MIT, as our own work.
- **Used here:** fourteen small EPUBs for the cases no public file exhibits: named entities beside a stylesheet link, a byte order mark, a chapter that does not parse, 30,000 nested elements, a remote spine href, Dublin Core roles and dates, a cover reference to an XHTML page, six legacy or UTF-16 encodings, and six spine media-type labels.
- **Modifications:** not applicable.

## Executable Books Project — MyST-NB fixtures

- **Source:** <https://github.com/executablebooks/MyST-NB>, `tests/notebooks`, at commit
  `23b1747ff9b7e408244c62dd3981238ea193b25a`.
- **License:** BSD-3-Clause; the upstream license is retained at
  `vendored/executablebooks-myst-nb/LICENSE`.
- **Used here:** `basic_unrun.md`, a MyST/Jupytext text notebook with notebook front matter and a
  code-cell directive; `with_eval.md`, which covers inline and block evaluation plus an evaluated
  figure; and `hide_cell_content.ipynb`, a Jupyter notebook whose cells carry `hide-input`,
  `hide-output`, and `hide-cell` tags.
- **Modifications:** none to the fixture bytes; files were relocated under
  `vendored/executablebooks-myst-nb/`.
