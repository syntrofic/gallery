import { reactive, watch } from 'vue'
import { account, albumPath, ApiError, commandKey, errorText, pathSegment, request, type Operation } from './api'

export interface LocalItem {
  id: string
  file: File
  name: string
  size: number
  percent: number
  state: 'waiting' | 'receiving' | 'validating' | 'committed' | 'failed' | 'canceled'
  error: string
  imageId?: string
}
export interface Batch {
  id: string
  album: string
  items: LocalItem[]
}
export const batches = reactive<Record<string, Batch>>({})
const requests = new Map<string, XMLHttpRequest>()
const stopped = new Set<string>()

export function uploadFile(batch: string, item: LocalItem): Promise<void> {
  return new Promise((resolve, reject) => {
    if (stopped.has(batch)) { reject(new Error('上传已取消。')); return }
    if (!navigator.onLine) { reject(new Error('网络已断开，恢复后可重试。')); return }
    const xhr = new XMLHttpRequest()
    const key = `${batch}/${item.id}`
    requests.set(key, xhr)
    xhr.open('PUT', `/api/v1/uploads/${pathSegment(batch)}/items/${pathSegment(item.id)}/body`)
    xhr.timeout = 300_000
    xhr.responseType = 'json'
    xhr.setRequestHeader('Content-Type', item.file.type || 'application/octet-stream')
    xhr.setRequestHeader('Accept', 'application/json')
    if (account.session.csrf_token) xhr.setRequestHeader('x-csrf-token', account.session.csrf_token)
    xhr.upload.onprogress = event => {
      item.percent = event.lengthComputable ? Math.floor(event.loaded / event.total * 100) : item.percent
      if (item.percent >= 100) item.state = 'validating'
    }
    xhr.upload.onload = () => { item.percent = 100; item.state = 'validating' }
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        item.imageId = xhr.response?.image_id
        item.state = 'committed'
        item.percent = 100
        resolve()
      } else reject(new ApiError(xhr.status, xhr.response || {}))
    }
    xhr.onerror = () => reject(new Error('网络连接已中断。'))
    xhr.ontimeout = () => reject(new Error('等待服务端确认超时。'))
    xhr.onabort = () => reject(new Error('上传已取消。'))
    xhr.onloadend = () => { requests.delete(key) }
    item.state = 'receiving'
    item.percent = 0
    item.error = ''
    xhr.send(item.file)
  })
}

export async function retryItem(batch: Batch, item: LocalItem) {
  try {
    const snapshot = await request<Operation>(`/uploads/${pathSegment(batch.id)}`)
    const remote = snapshot.items?.find(entry => entry.id === item.id || entry.upload_item_id === item.id)
    if (remote?.state === 'committed') {
      item.state = 'committed'
      item.imageId = remote.image_id
      item.percent = 100
      return
    }
    if (snapshot.state === 'canceled' || ['receiving', 'validating', 'canceled'].includes(remote?.state || '')) {
      throw new Error('此文件仍由服务端处理，或批次已取消。请刷新状态。')
    }
    await uploadFile(batch.id, item)
  } catch (failure) {
    item.state = stopped.has(batch.id) ? 'canceled' : 'failed'
    item.error = errorText(failure)
  }
}

export async function createBatch(album: string, files: File[]) {
  const candidates = files.map(file => ({ id: commandKey(), file }))
  const result = await request<{ id: string; items: { client_item_id: string; upload_item_id: string }[] }>(
    `${albumPath(album)}/uploads`, {
      method: 'POST', idempotencyKey: commandKey(),
      body: { items: candidates.map(candidate => ({
        client_item_id: candidate.id, name: candidate.file.name, size: candidate.file.size,
      })) },
    },
  )
  const byId = new Map(result.items.map(item => [item.client_item_id, item.upload_item_id]))
  const id = result.id
  batches[id] = {
    id, album,
    items: candidates.map(({ id, file }) => ({
      id: byId.get(id)!, file, name: file.name, size: file.size, percent: 0, state: 'waiting', error: '',
    })),
  }
  const batch = batches[id]!
  let next = 0
  const worker = async () => {
    while (next < batch.items.length && !stopped.has(id)) {
      const item = batch.items[next++]!
      try { await uploadFile(id, item) }
      catch (failure) { item.state = stopped.has(id) ? 'canceled' : 'failed'; item.error = errorText(failure) }
    }
  }
  const concurrency = Math.min(8, Math.max(1, account.capabilities?.upload_concurrency_hint || 3), files.length)
  void Promise.all(Array.from({ length: concurrency }, worker)).then(() => { account.event++ })
  return id
}

export async function cancelBatch(id: string) {
  stopped.add(id)
  for (const [key, xhr] of requests) if (key.startsWith(id + '/')) xhr.abort()
  for (const item of batches[id]?.items || []) if (item.state === 'waiting') item.state = 'canceled'
  return request<Operation>(`/uploads/${pathSegment(id)}`, { method: 'DELETE' })
}

watch(() => account.session.actor?.id, (id, previous) => {
  if (!previous || id === previous) return
  for (const id of Object.keys(batches)) {
    stopped.add(id)
    delete batches[id]
  }
  for (const xhr of requests.values()) xhr.abort()
})
