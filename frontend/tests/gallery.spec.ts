import { test, expect, type Page } from '@playwright/test'

const password = process.env.GALLERY_TEST_PASSWORD || 'gallery-browser-test-26'
const image = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAACAAAAAYCAIAAAAUMWhjAAAAbElEQVR4nLXCERvCAABF0YfDMByGYTgMh2HwIBwOw2EYhuEwur+tX3HPd5LiHoo6h6LOsagzFnVORZ1zUedS1JmKOteizlzUuRV17kWdR1FnKeqsRZ1nUWcr6ryKOu+izqeo8y3q7EWdX1H/AfTSej16KIgMAAAAAElFTkSuQmCC', 'base64')

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
  const batch = await page.request.post(`/api/v1/albums/${id}/uploads`, {
    headers, data: { items: [{ client_item_id: 'browser-fixture', name: '测试照片.png', size: image.byteLength }] },
  })
  expect(batch.ok()).toBeTruthy()
  const selected = await batch.json()
  const put = await page.request.put(`/api/v1/uploads/${selected.id}/items/${selected.items[0].upload_item_id}/body`, {
    headers: { ...headers, 'content-type': 'image/png' }, data: image,
  })
  expect(put.ok(), await put.text()).toBeTruthy()
  return { id, imageId: (await put.json()).image_id as string }
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
  expect(originals).toEqual([])
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
