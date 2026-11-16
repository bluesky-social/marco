// `npm run explainer`: draws docs/storage-explainer.html in Chromium, fails on page or console
// errors, and saves each widget's first drawing between its <!--pre--> markers, so the page reads
// the same where scripts don't run (marco's /docs serves standalone pages with a no-script CSP).
// The lookup stepper is saved with every step of its first lookup, one under the other.

import { chromium } from 'playwright'
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath, pathToFileURL } from 'node:url'

const file = fileURLToPath(new URL('../../docs/storage-explainer.html', import.meta.url))
const IDS = ['did-out', 'out-out', 'ef-out', 'sz-out', 'dl-out', 'mem-out', 'map', 'st-out']

const browser = await chromium.launch()
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } })
const errors = []
page.on('pageerror', (e) => errors.push(e.message))
page.on('console', (m) => ['error', 'warning'].includes(m.type()) && errors.push(m.text()))
await page.goto(pathToFileURL(file).href)
const steps = []
for (;;) {
  steps.push(await page.$eval('#st-out .stepbox', (e) => e.outerHTML))
  if (await page.$eval('#st-next', (b) => b.disabled)) break
  await page.click('#st-next')
}
const counters = await page.$eval('#st-out .counters', (e) => e.outerHTML)
await page.click('#st-back')
while (!(await page.$eval('#st-back', (b) => b.disabled))) await page.click('#st-back')
const html = Object.fromEntries(await Promise.all(IDS.map(async (id) => [id, await page.$eval(`#${id}`, (e) => e.innerHTML)])))
html['st-out'] = steps.join('\n') + counters
await browser.close()
if (errors.length) {
  console.error(errors.join('\n'))
  process.exit(1)
}

let src = readFileSync(file, 'utf8')
for (const id of IDS) {
  const re = new RegExp(`(id="${id}"[^>]*>)<!--pre-->[\\s\\S]*?<!--/pre-->`)
  if (!re.test(src)) throw new Error(`#${id} has no <!--pre--> markers`)
  src = src.replace(re, (_, open) => `${open}<!--pre-->${html[id]}<!--/pre-->`)
}
writeFileSync(file, src)
console.log(`saved ${IDS.length} widgets' first drawing in ${file}`)
