import { readFile } from 'node:fs/promises'
import { test, expect, type Page } from '@playwright/test'

const password = process.env.GALLERY_TEST_PASSWORD || 'gallery-browser-test-26'
const image = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAACAAAAAYCAIAAAAUMWhjAAAAbElEQVR4nLXCERvCAABF0YfDMByGYTgMh2HwIBwOw2EYhuEwur+tX3HPd5LiHoo6h6LOsagzFnVORZ1zUedS1JmKOteizlzUuRV17kWdR1FnKeqsRZ1nUWcr6ryKOu+izqeo8y3q7EWdX1H/AfTSej16KIgMAAAAAElFTkSuQmCC', 'base64')
const portrait = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAABgAAAAoCAIAAABrb60CAAAAk0lEQVR4nK3CjVkBAABF0Tffm+MOkVQUFSHkL0Wtaol7vpO7osygKHNflBkWZR6KMo9FmaeizKgoMy7KPBdlXooyk6LMtCjzWpR5K8q8F2VmRZl5UeajKLMoyiyLMquizGdRZl2U2RRltkWZr6LMriizL8ocijLHosypKPNdlDkXZX6KMr9FmUtR5lqU+SvK/BflDV90l92E+4qqAAAAAElFTkSuQmCC', 'base64')

async function login(page: Page) {
  await page.goto('/login')
  await expect(page.getByRole('heading', { name: '登录图库' })).toBeVisible()
  await page.getByLabel('用户名').fill('admin')
  await page.getByLabel('密码', { exact: true }).fill(password)
  await page.getByRole('button', { name: '登录', exact: true }).click()
  await expect(page).toHaveURL(/\/albums$/)
}

async function seedPhoto(page: Page, album: string) {
  const session = await page.request.get('/api/v1/session')
  const csrf = (await session.json()).csrf_token
  const headers = { 'x-csrf-token': csrf }
  const made = await page.request.post('/api/v1/albums', { headers, data: { name: album } })
  expect(made.ok()).toBeTruthy()
  const { id } = await made.json() as { id: string }
  return { id, imageId: await addPhoto(page, headers, id, '测试照片.png', image) }
}

async function addPhoto(page: Page, headers: Record<string, string>, id: string, name: string, body: Buffer, mime = 'image/png') {
  const batch = await page.request.post(`/api/v1/albums/${id}/uploads`, {
    headers, data: { items: [{ client_item_id: 'browser-fixture', name, size: body.byteLength }] },
  })
  expect(batch.ok()).toBeTruthy()
  const selected = await batch.json()
  const put = await page.request.put(`/api/v1/uploads/${selected.id}/items/${selected.items[0].upload_item_id}/body`, {
    headers: { ...headers, 'content-type': mime }, data: body,
  })
  expect(put.ok(), await put.text()).toBeTruthy()
  return (await put.json()).image_id as string
}

test('desktop creates, uploads, opens previews and downloads the original ZIP', async ({ page }, info) => {
  test.skip(info.project.name !== 'desktop')
  const browserErrors: string[] = []
  page.on('pageerror', error => browserErrors.push(error.message))
  const originals: string[] = []
  page.on('request', req => { if (req.url().endsWith('/original-view')) originals.push(req.url()) })
  await login(page)
  await page.getByRole('button', { name: '新建相册' }).first().click()
  const dialog = page.getByRole('dialog', { name: '新建相册' })
  await dialog.getByLabel('名称').fill('桌面回归相册')
  await dialog.getByLabel('描述').fill('独立自托管相册')
  await dialog.getByRole('button', { name: '创建相册' }).click()
  await expect(page.getByRole('heading', { name: '桌面回归相册', level: 1 })).toBeVisible()
  await expect(page).toHaveURL(/\/albums\/[^/]+$/)
  const album = new URL(page.url()).pathname
  await page.getByRole('link', { name: '上传照片' }).click()
  await page.locator('input[type=file]').setInputFiles({ name: '桌面原图.png', mimeType: 'image/png', buffer: image })
  await page.getByRole('button', { name: /上传\s*1\s*张照片/ }).click()
  await expect(page.getByText('已入册', { exact: true })).toBeVisible()
  await page.getByRole('link', { name: '查看相册' }).click()
  const tile = page.getByRole('link', { name: '桌面原图.png' })
  await expect(tile.locator('img')).toBeVisible()
  await tile.click()
  await expect(page.getByRole('heading', { name: '桌面原图.png' })).toBeVisible()
  await expect(page.locator('.viewer-stage img')).toBeVisible()
  await page.emulateMedia({ colorScheme: 'dark' })
  await expect(page.locator('.viewer-stage img')).toHaveCSS('filter', 'brightness(0.68)')
  expect(originals).toEqual([])
  const [download] = await Promise.all([
    page.waitForEvent('download'),
    page.getByRole('link', { name: '下载原图', exact: true }).click(),
  ])
  expect(download.suggestedFilename()).toBe('桌面原图.png')
  expect(await readFile((await download.path())!)).toEqual(image)
  await page.getByRole('button', { name: '查看原图' }).click()
  await expect.poll(() => originals.length).toBe(1)
  await page.reload()
  await expect(page.locator('.viewer-stage img')).toBeVisible()
  await expect(page.getByRole('button', { name: '查看原图' })).toBeVisible()
  await page.getByRole('button', { name: '编辑描述' }).click()
  const edit = page.getByRole('dialog', { name: '编辑照片描述' })
  await edit.getByLabel('描述').fill('桌面测试照片')
  await edit.getByRole('button', { name: '保存描述' }).click()
  await expect(page.getByText('桌面测试照片', { exact: true })).toBeVisible()
  await page.goto(album)
  await page.getByRole('button', { name: '整册 ZIP' }).click()
  const archive = page.getByRole('link', { name: '下载原图 ZIP' })
  await expect(archive).toBeVisible()
  const response = await page.request.get((await archive.getAttribute('href'))!)
  expect(response.status()).toBe(200)
  expect((await response.body()).subarray(0, 2).toString()).toBe('PK')
  await page.goto('/albums')
  await page.getByRole('searchbox', { name: '搜索相册' }).fill('桌面测试照片')
  await page.getByRole('button', { name: '搜索', exact: true }).click()
  await expect(page.getByRole('heading', { name: '匹配的照片' })).toBeVisible()
  await expect(page.getByRole('link', { name: /桌面原图.png/ }).first()).toBeVisible()
  expect(browserErrors).toEqual([])
})

test('mobile keeps original downloads per image and preserves the deep link', async ({ page }, info) => {
  test.skip(info.project.name !== 'mobile')
  await login(page)
  const { id, imageId } = await seedPhoto(page, '手机回归相册')
  await page.goto(`/albums/${id}`)
  await expect(page.getByRole('heading', { name: '手机回归相册' })).toBeVisible()
  await expect(page.getByRole('button', { name: '整册 ZIP' })).toHaveCount(0)
  await page.getByRole('button', { name: '选择照片' }).click()
  await page.getByRole('button', { name: '测试照片.png' }).click()
  await page.getByRole('button', { name: '获取原图下载' }).click()
  const claim = page.getByRole('link', { name: '下载原图' })
  await expect(claim).toBeVisible()
  const download = await page.request.get((await claim.getAttribute('href'))!)
  expect(await download.body()).toEqual(image)
  await page.goto(`/albums/${id}/images/${imageId}`)
  await expect(page.getByRole('heading', { name: '测试照片.png' })).toBeVisible()
  await expect(page.getByRole('button', { name: '查看原图' })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390)
})

test('served assets have immutable caching and API misses stay non-HTML', async ({ request }, info) => {
  test.skip(info.project.name !== 'desktop')
  const page = await request.get('/albums')
  expect(page.status()).toBe(200)
  expect(page.headers()['cache-control']).toBe('no-cache')
  const asset = (await page.text()).match(/src="(\/assets\/[^"]+\.js)"/)?.[1]
  expect(asset).toBeTruthy()
  const hit = await request.get(asset!)
  expect(hit.status()).toBe(200)
  expect(hit.headers()['cache-control']).toContain('immutable')
  const miss = await request.get('/api/v1/does-not-exist', { headers: { Accept: 'text/html' } })
  expect(miss.status()).toBe(404)
  expect(miss.headers()['content-type']).not.toContain('text/html')
  expect((await request.get('/assets/missing.css')).status()).toBe(404)
})

test('the public portfolio follows system appearance and opens an accessible lightbox over dense uncropped photos', async ({ page, browser }, info) => {
  await login(page)
  const publicAlbum = await seedPhoto(page, '公开作品相册')
  const token = (await (await page.request.get('/api/v1/session')).json()).csrf_token
  const headers = { 'x-csrf-token': token }
  const capabilities = await page.request.get('/api/v1/capabilities')
  expect((await capabilities.json()).ai_enabled).toBe(false)
  const scan = await page.request.post('/api/v1/ai/scans', { headers, data: { scope: {} } })
  expect(scan.status()).toBe(503)
  expect((await scan.json()).code).toBe('FEATURE_GATED')
  const portraitId = await addPhoto(page, headers, publicAlbum.id, '测试竖图.png', portrait)
  const hidden = await page.request.post('/api/v1/albums', { headers, data: { name: '未公开的手稿' } })
  expect(hidden.ok()).toBeTruthy()
  const hiddenAlbum = await hidden.json()
  const worksBefore = await (await page.request.get('/api/v1/public/works')).json()
  expect(worksBefore.items.some((item: { album_id: string }) => item.album_id === publicAlbum.id)).toBe(false)
  const currentAlbum = await (await page.request.get(`/api/v1/albums/${publicAlbum.id}`)).json()
  const shown = await page.request.patch(`/api/v1/albums/${publicAlbum.id}/access`, {
    headers, data: { visibility: 'public', expected_revision: currentAlbum.revision },
  })
  expect(shown.ok(), await shown.text()).toBeTruthy()
  await page.goto('/settings/general')
  await expect(page.getByRole('heading', { name: '图库助手' })).toHaveCount(0)
  await page.getByLabel('画廊名称').fill('测试作品集')
  await page.getByLabel('简介').fill('一组公开作品。')
  await page.getByRole('button', { name: '保存公开资料' }).click()
  await expect(page.getByText('公开画廊资料已保存。')).toBeVisible()
  await page.goto('/settings/limits')
  await expect(page.getByLabel('每批上传上限')).toBeVisible()
  await expect(page.getByLabel('每日 AI Token 预算')).toHaveCount(0)
  await page.goto('/agent')
  await expect(page).toHaveURL(/\/albums$/)

  const visitor = await browser.newPage({
    baseURL: new URL(page.url()).origin, colorScheme: 'dark',
    isMobile: info.project.name === 'mobile', hasTouch: info.project.name === 'mobile',
    viewport: info.project.name === 'mobile' ? { width: 390, height: 844 } : { width: 1280, height: 900 },
  })
  try {
    const originals: string[] = []
    visitor.on('request', request => { if (/original-(view|download)$/.test(request.url())) originals.push(request.url()) })
    await visitor.goto('/')
    await expect(visitor.getByRole('heading', { name: '测试作品集', level: 1 })).toBeVisible()
    await expect(visitor.locator('.public-shell')).toHaveCSS('background-color', 'rgb(18, 19, 20)')
    const collection = visitor.locator(`a[href^="/albums/${publicAlbum.id}?"]`)
    await expect(collection).toBeVisible()
    await expect(collection.locator('img').first()).toHaveCSS('filter', 'brightness(0.68)')
    const featured = visitor.locator('.portfolio-feature')
    await expect(featured.locator('img')).toBeVisible()
    await expect(featured.locator('img')).toHaveCSS('filter', 'brightness(0.68)')
    await featured.click()
    const lightbox = visitor.getByRole('dialog', { name: '照片查看器' })
    await expect(lightbox.locator('img.is-ready')).toBeVisible()
    await expect(lightbox).toHaveCSS('background-color', 'rgb(18, 19, 20)')
    await expect(lightbox.locator('img.is-ready')).toHaveCSS('filter', 'brightness(0.68)')
    await visitor.emulateMedia({ colorScheme: 'light' })
    await expect(lightbox).toHaveCSS('background-color', 'rgb(255, 255, 255)')
    await expect(lightbox.locator('img.is-ready')).toBeVisible()
    await expect(lightbox.locator('img.is-ready')).toHaveCSS('filter', 'none')
    const openedUrl = visitor.url()
    const displayUrl = await lightbox.locator('img.is-ready').evaluate(img => (img as HTMLImageElement).currentSrc)
    const blockedTransfer = (route: import('@playwright/test').Route) => route.abort()
    await visitor.route(displayUrl, blockedTransfer)
    await lightbox.getByRole('button', { name: '下载照片' }).click()
    await expect(lightbox.getByText('下载失败，请重试。')).toBeVisible()
    await visitor.unroute(displayUrl, blockedTransfer)
    const [previewDownload] = await Promise.all([
      visitor.waitForEvent('download'),
      lightbox.getByRole('button', { name: '下载照片' }).click(),
    ])
    const displayedPhoto = await visitor.request.get(displayUrl)
    expect(previewDownload.suggestedFilename()).toMatch(/\.(jpg|png|webp|avif)$/)
    expect(await readFile((await previewDownload.path())!)).toEqual(await displayedPhoto.body())
    await expect(visitor).toHaveURL(openedUrl)
    await lightbox.getByRole('button', { name: '关闭查看器' }).click()
    await expect(visitor).toHaveURL(/\/$/)
    await expect(visitor.getByText('未公开的手稿')).toHaveCount(0)
    await expect(visitor.getByRole('link', { name: '管理' })).toHaveAttribute('href', '/admin')
    expect(await visitor.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe('rgb(255, 255, 255)')
    await collection.click()
    await expect(visitor.getByLabel('筛选照片')).toHaveCount(0)
    await visitor.setViewportSize({ width: 390, height: 360 })
    const albumTile = visitor.getByRole('link', { name: '测试照片.png' })
    await expect(albumTile.locator('img')).toBeVisible()
    const portraitTile = visitor.getByRole('link', { name: '测试竖图.png' })
    await expect(portraitTile.locator('img')).toBeVisible()
    const landscapeBox = (await albumTile.boundingBox())!
    const portraitBox = (await portraitTile.boundingBox())!
    expect(portraitBox.width / portraitBox.height).toBeCloseTo(24 / 40, 2)
    expect(portraitBox.x - landscapeBox.x - landscapeBox.width).toBeCloseTo(4, 0)
    await albumTile.scrollIntoViewIfNeeded()
    const albumScroll = await visitor.evaluate(() => window.scrollY)
    await albumTile.click()
    await expect(lightbox.locator('img.is-ready')).toBeVisible()
    if (info.project.name === 'mobile') {
      const stage = lightbox.locator('.lightbox-stage')
      const track = lightbox.locator('.lightbox-track')
      const nextSlide = lightbox.locator('.lightbox-slide[aria-hidden="true"] img.is-visible')
      await expect(nextSlide).toHaveCount(1)
      const neighbor = await nextSlide.elementHandle()
      await stage.evaluate(element => {
        // WebKit's Playwright touchscreen can only tap. Drive pointer sequences directly.
        element.setPointerCapture = () => {}
      })
      const touch = { pointerId: 1, pointerType: 'touch', button: 0, clientY: 145, bubbles: true }
      await stage.dispatchEvent('pointerdown', { ...touch, clientX: 240 })
      await stage.dispatchEvent('pointermove', { ...touch, clientX: 80 })
      await expect.poll(() => track.evaluate(element => new DOMMatrix(getComputedStyle(element).transform).m41)).toBe(-160)
      expect(new URL(visitor.url()).searchParams.get('photo')).toBe(publicAlbum.imageId)
      await stage.dispatchEvent('pointerup', { ...touch, clientX: 80 })
      await expect.poll(() => new URL(visitor.url()).searchParams.get('photo')).toBe(portraitId)
      await expect(lightbox.locator('img.is-ready')).toBeVisible()
      expect(await lightbox.locator('img.is-ready').evaluate((img, preloaded) => img === preloaded, neighbor)).toBe(true)
      await lightbox.getByRole('button', { name: '上一张' }).click()
      await expect.poll(() => new URL(visitor.url()).searchParams.get('photo')).toBe(publicAlbum.imageId)
    }
    await lightbox.getByRole('button', { name: '放大' }).click()
    await expect(lightbox.getByRole('button', { name: '适合屏幕' })).toHaveAttribute('aria-pressed', 'true')
    await visitor.keyboard.press('ArrowRight')
    await expect.poll(() => new URL(visitor.url()).searchParams.get('photo')).toBe(portraitId)
    await expect(lightbox.locator('img.is-ready')).toBeVisible()
    await visitor.keyboard.press('Escape')
    await expect(lightbox).toHaveCount(0)
    expect(await visitor.evaluate(() => window.scrollY)).toBeCloseTo(albumScroll, -1)
    await expect(visitor.getByText('测试照片.png', { exact: true })).toHaveCount(0)
    await visitor.getByRole('link', { name: '作品', exact: true }).click()
    await expect(visitor).toHaveURL(/\/photos$/)
    await expect(visitor.getByLabel('筛选相册')).toHaveCount(0)
    await visitor.emulateMedia({ colorScheme: 'dark' })
    await expect(visitor.locator('.public-shell')).toHaveCSS('background-color', 'rgb(18, 19, 20)')
    const work = visitor.getByRole('link', { name: '测试照片.png' })
    await expect(work).toBeVisible()
    await expect(work.locator('img')).toHaveCSS('filter', 'brightness(0.68)')
    await expect(visitor.getByText('测试照片.png', { exact: true })).toHaveCount(0)
    await visitor.setViewportSize({ width: 390, height: 844 })
    await expect(visitor.locator('.photo-wall')).toHaveAttribute('data-columns', '2')
    expect(await visitor.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390)
    await work.click()
    await expect(lightbox.locator('img.is-ready')).toBeVisible()
    await expect(visitor.getByText('测试照片.png', { exact: true })).toHaveCount(0)
    await visitor.goto(`/albums/${publicAlbum.id}/images/${publicAlbum.imageId}`)
    await expect(lightbox.locator('img.is-ready')).toBeVisible()
    await expect(visitor.getByText('测试照片.png', { exact: true })).toHaveCount(0)
    expect(originals).toEqual([])
    const privateWorks = await (await visitor.request.get(`/api/v1/public/works?album_id=${hiddenAlbum.id}`)).json()
    expect(privateWorks.items).toEqual([])
    expect((await visitor.request.get(`/api/v1/albums/${hiddenAlbum.id}`)).status()).toBe(404)
  } finally { await visitor.close() }
})

test('the photo wall follows EXIF orientation and zoom requests a larger preview', async ({ page, browser }) => {
  await login(page)
  const { id } = await seedPhoto(page, '预览分辨率回归')
  const headers = { 'x-csrf-token': (await (await page.request.get('/api/v1/session')).json()).csrf_token }
  const encoded = await page.evaluate(() => {
    const canvas = document.createElement('canvas')
    canvas.width = 1440
    canvas.height = 1920
    canvas.getContext('2d')!.fillRect(0, 0, canvas.width, canvas.height)
    return canvas.toDataURL('image/jpeg').split(',')[1]
  })
  const jpeg = Buffer.from(encoded, 'base64')
  // APP1 EXIF little-endian TIFF with Orientation=6 (90° clockwise).
  const orientation = Buffer.from('ffe1002245786966000049492a0008000000010012010300010000000600000000000000', 'hex')
  const imageId = await addPhoto(page, headers, id, '旋转预览.jpg', Buffer.concat([jpeg.subarray(0, 2), orientation, jpeg.subarray(2)]), 'image/jpeg')
  const path = `/api/v1/albums/${id}/images/${imageId}`
  const metadata = async () => (await page.request.get(path)).json()
  await expect.poll(async () => (await metadata()).variants.map((variant: { spec_id: string }) => variant.spec_id)).toEqual(
    expect.arrayContaining(['mobile', 'desktop']),
  )
  const previews = await metadata()
  expect(previews.width).toBe(1440)
  expect(previews.height).toBe(1920)
  const mobile = previews.variants.find((variant: { spec_id: string }) => variant.spec_id === 'mobile')
  const desktop = previews.variants.find((variant: { spec_id: string }) => variant.spec_id === 'desktop')
  expect(mobile.width).toBe(960)
  expect(desktop.width).toBe(1920)
  const album = await (await page.request.get(`/api/v1/albums/${id}`)).json()
  expect((await page.request.patch(`/api/v1/albums/${id}/access`, {
    headers, data: { visibility: 'public', expected_revision: album.revision },
  })).ok()).toBeTruthy()

  const visitor = await browser.newPage({ baseURL: new URL(page.url()).origin, viewport: { width: 390, height: 844 }, deviceScaleFactor: 2 })
  try {
    await visitor.goto(`/albums/${id}?portfolio=1`)
    const tile = visitor.getByRole('link', { name: '旋转预览.jpg' })
    await expect(tile.locator('img')).toBeVisible()
    const box = (await tile.boundingBox())!
    expect(box.width / box.height).toBeCloseTo(4 / 3, 2)
    await tile.click()
    const viewer = visitor.getByRole('dialog', { name: '照片查看器' })
    const preview = viewer.locator('img.is-ready')
    await expect.poll(() => preview.evaluate(img => (img as HTMLImageElement).currentSrc)).toContain(`/variants/${mobile.id}/body`)
    await viewer.getByRole('button', { name: '放大' }).click()
    const larger = previews.variants.filter((variant: { width: number }) => variant.width > mobile.width)
    await expect.poll(async () => {
      const source = await preview.evaluate(img => (img as HTMLImageElement).currentSrc)
      return larger.some((variant: { id: string }) => source.endsWith(`/variants/${variant.id}/body`))
    }).toBe(true)
    await expect(viewer.getByRole('button', { name: '适合屏幕' })).toHaveAttribute('aria-pressed', 'true')
  } finally { await visitor.close() }
})
