# firecrawl-pdf-inspector

Fast PDF classification, text extraction, and image extraction for Node.js/Bun. Native Rust performance via [napi-rs](https://napi.rs).

Built by [Firecrawl](https://firecrawl.dev) for hybrid OCR pipelines — extract text from PDF structure where possible, fall back to OCR only when needed.

## Install

```bash
npm install firecrawl-pdf-inspector
# or
bun add firecrawl-pdf-inspector
```

Prebuilt binaries included for **linux-x64** and **macOS ARM64**. No Rust toolchain needed.

## API

### `processPdf(buffer: Buffer, pages?: number[]): PdfResult`

Full PDF processing — classify, extract text, and convert to Markdown in one call.

```typescript
import { processPdf } from 'firecrawl-pdf-inspector'
import { readFileSync } from 'fs'

const pdf = readFileSync('document.pdf')
const result = processPdf(pdf)

console.log(result.pdfType)    // "TextBased" | "Scanned" | "Mixed" | "ImageBased"
console.log(result.markdown)   // Markdown string or null
console.log(result.pageCount)  // 42
```

### `classifyPdf(buffer: Buffer): PdfClassification`

Classify a PDF as TextBased, Scanned, Mixed, or ImageBased (~10-50ms). Returns which pages need OCR.

```typescript
import { classifyPdf } from 'firecrawl-pdf-inspector'

const result = classifyPdf(readFileSync('document.pdf'))

console.log(result.pdfType)         // "TextBased" | "Scanned" | "Mixed" | "ImageBased"
console.log(result.pageCount)       // 42
console.log(result.pagesNeedingOcr) // [5, 12, 15] (0-indexed)
console.log(result.confidence)      // 0.875
```

### `processPdfWithImages(buffer: Buffer, pages?: number[]): PdfResultWithImages`

Process a PDF and extract both markdown and images in one call. The markdown contains `![image](pdf-image://N)` placeholders where `N` is the index into the returned `images` array.

```typescript
import { processPdfWithImages } from 'firecrawl-pdf-inspector'
import { readFileSync } from 'fs'

const result = processPdfWithImages(readFileSync('document.pdf'))

console.log(result.images.length)  // 110
console.log(result.markdown)       // "# Title\n\n![image](pdf-image://0)\n\nSome text..."

// Replace placeholders with real URLs after uploading
let content = result.markdown
for (let i = 0; i < result.images.length; i++) {
  const img = result.images[i]
  const ext = img.format === 'Jpeg' ? 'jpg' : 'png'
  const url = await uploadImage(img.data, `image-${i}.${ext}`)
  content = content.replace(`pdf-image://${i}`, url)
}
```

### `extractImages(buffer: Buffer): ExtractedImage[]`

Extract embedded images from a PDF as raw bytes. Supports JPEG (DCTDecode) and PNG (FlateDecode) images. Returns image data with page position and dimensions.

Image extraction is a separate function from text extraction — calling `processPdf` or `extractText` does **not** pay the cost of image decompression/encoding.

```typescript
import { extractImages } from 'firecrawl-pdf-inspector'
import { writeFileSync } from 'fs'

const images = extractImages(readFileSync('document.pdf'))

for (const img of images) {
  const ext = img.format === 'Jpeg' ? 'jpg' : 'png'
  writeFileSync(`page${img.page}_${img.width}x${img.height}.${ext}`, img.data)
  console.log(`Page ${img.page}: ${img.width}x${img.height} ${img.format}`)
}
```

### `extractTextInRegions(buffer: Buffer, pageRegions: PageRegions[]): PageRegionTexts[]`

Extract text within bounding-box regions from a PDF. Designed for hybrid OCR pipelines where a layout model detects regions in rendered page images, and this function extracts text from the PDF structure for text-based pages — skipping GPU OCR.

Each region result includes a `needsOcr` flag that signals unreliable extraction (empty text, GID-encoded fonts, garbage text, encoding issues).

```typescript
import { extractTextInRegions } from 'firecrawl-pdf-inspector'

const result = extractTextInRegions(pdf, [
  {
    page: 0, // 0-indexed
    regions: [
      [0, 0, 300, 400],    // [x1, y1, x2, y2] in PDF points, top-left origin
      [300, 0, 612, 400],
    ]
  }
])

for (const region of result[0].regions) {
  if (region.needsOcr) {
    // Unreliable text — send this region to OCR instead
  } else {
    console.log(region.text) // Extracted text in reading order
  }
}
```

### `extractText(buffer: Buffer): string`

Extract plain text from a PDF.

```typescript
import { extractText } from 'firecrawl-pdf-inspector'

const text = extractText(readFileSync('document.pdf'))
```

## Types

```typescript
interface PdfResult {
  pdfType: string          // "TextBased" | "Scanned" | "Mixed" | "ImageBased"
  markdown: string | null  // Markdown output
  pageCount: number
  processingTimeMs: number
  pagesNeedingOcr: number[] // 1-indexed page numbers
  title: string | null
  confidence: number        // 0.0 - 1.0
  isComplexLayout: boolean
  pagesWithTables: number[]
  pagesWithColumns: number[]
  hasEncodingIssues: boolean
}

// Extends PdfResult with extracted images
interface PdfResultWithImages extends PdfResult {
  images: ExtractedImage[]  // Indices match pdf-image://N placeholders in markdown
}

interface PdfClassification {
  pdfType: string          // "TextBased" | "Scanned" | "Mixed" | "ImageBased"
  pageCount: number
  pagesNeedingOcr: number[] // 0-indexed page numbers
  confidence: number        // 0.0 - 1.0
}

interface ExtractedImage {
  page: number              // 1-indexed page number
  x: number                 // X position on page
  y: number                 // Y position on page
  width: number             // Pixel width
  height: number            // Pixel height
  format: string            // "Jpeg" | "Png"
  data: Buffer              // Raw image bytes (valid JPEG or PNG file)
}

interface PageRegions {
  page: number              // 0-indexed
  regions: number[][]       // [[x1, y1, x2, y2], ...] in PDF points, top-left origin
}

interface PageRegionTexts {
  page: number
  regions: RegionText[]
}

interface RegionText {
  text: string
  needsOcr: boolean         // true when text is unreliable
}
```

## Supported image formats

| PDF Filter    | Output Format | Status    |
|---------------|---------------|-----------|
| DCTDecode     | JPEG          | Supported |
| FlateDecode   | PNG           | Supported |
| JPXDecode     | JPEG2000      | Planned   |
| CCITTFaxDecode| TIFF          | Planned   |

**Color spaces:** DeviceRGB, DeviceGray, DeviceCMYK (converted to RGB), Indexed, ICCBased, CalRGB, CalGray.

**Note:** Vector graphics (drawn with PDF path operators) are not raster images and cannot be extracted — they would need to be rendered.

## Performance

Text extraction and image extraction are independent paths. `processPdf` and `extractText` skip image processing entirely, so there is zero overhead when you only need text.

## Platforms

| Platform | Architecture | Supported |
|----------|-------------|-----------|
| Linux    | x64         | Yes       |
| macOS    | ARM64       | Yes       |

## License

MIT
