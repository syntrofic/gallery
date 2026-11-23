import { reactive } from 'vue'

export interface Actor {
  id: string
  username: string
  role: 'owner' | 'collaborator'
  must_change_password: boolean
}
export interface Session {
  actor: Actor | null
  csrf_token?: string
  event_cursor?: number
}
export interface Capabilities {
  ai_enabled: boolean
  max_upload_items: number
  upload_concurrency_hint: number
  max_download_selection: number
}
export interface Album {
  id: string
  name: string
  description: string
  cover_image_id: string | null
  visibility: 'public' | 'private'
  archive_enabled: boolean
  revision: number
  content_revision: number
  acl_revision: number
  grants?: { actor_id: string; browse: boolean; upload: boolean; edit: boolean; view_original: boolean; download: boolean }[]
  capabilities: Record<'browse' | 'edit' | 'upload' | 'view_original' | 'download' | 'archive_enabled', boolean>
}
export interface Variant {
  id: string
  spec_id: string
  width: number | null
  height: number | null
}
export interface Photo {
  id: string
  display_name: string
  width: number
  height: number
  variants?: Variant[]
  placeholder?: boolean
  can_inline_original?: boolean
  previous_image_id?: string | null
  next_image_id?: string | null
  selection?: Record<string, { variant_id: string } | null>
  srcset?: { variant_id: string; width: number | null }[]
  caption?: { source: string; text: string }
  lifecycle_revision?: number
  revision?: number
}
export interface Presentation { title: string; description: string; revision: number }
export interface Work extends Photo { album_id: string; variants: Variant[] }
export interface Collection { id: string; name: string; description: string; count: number; covers: Work[] }
export interface Page<T> { items: T[]; next_cursor: string | null }
export interface Job {
  id: string
  kind: string
  state: string
  progress: { done: number; total: number }
  failure?: string | null
}
export interface Operation {
  id: string
  state: string
  album_id?: string
  items?: UploadEntry[]
  job_id?: string | null
  status_url?: string
  body_url?: string | null
  failure_code?: string | null
  missing?: string[]
}
export interface UploadEntry {
  id?: string
  upload_item_id?: string
  client_item_id: string
  name?: string
  size?: number
  state: string
  image_id?: string
  failure_code?: string
}

interface Problem { detail?: string; code?: string; retryable?: boolean; request_id?: string }
export class ApiError extends Error {
  readonly status: number
  readonly problem: Problem
  constructor(status: number, problem: Problem = {}) {
    super(problem.detail || `HTTP ${status}`)
    this.status = status
    this.problem = problem
  }
}

export const account = reactive({
  initialized: true,
  presentation: { title: "Gallery", description: "", revision: 0 } as Presentation,
  session: { actor: null } as Session,
  capabilities: null as Capabilities | null,
  event: 0,
})

export function visibleCaption(photo?: Photo) {
  const caption = photo?.caption
  return caption?.source === 'model' && !account.capabilities?.ai_enabled ? undefined : caption
}

export function commandKey() {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), part => part.toString(16).padStart(2, '0')).join('')
}
export function pathSegment(value: string) { return encodeURIComponent(value) }
export function albumPath(id: string) { return `/albums/${pathSegment(id)}` }
export function photoPath(album: string, photo: string) { return `${albumPath(album)}/images/${pathSegment(photo)}` }
export function mediaPath(album: string | undefined, photo: string, variant: string) {
  const base = album ? photoPath(album, photo) : `/library/images/${pathSegment(photo)}`
  return `/api/v1${base}/variants/${pathSegment(variant)}/body`
}
export function query(params: Record<string, string | number | boolean | null | undefined>) {
  const fields = new URLSearchParams()
  for (const [name, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && value !== '') fields.set(name, String(value))
  }
  return fields.size ? `?${fields}` : ''
}
export function safeDestination(raw: unknown, fallback = '/albums') {
  if (typeof raw !== 'string' || !raw.startsWith('/') || raw.startsWith('//') ||
      /[\\\u0000-\u0020]/u.test(raw)) return fallback
  const url = new URL(raw, location.origin)
  if (url.origin !== location.origin || /^\/(?:api|assets|health)(?:\/|$)/.test(url.pathname)) return fallback
  return url.pathname + url.search + url.hash
}

export async function request<T>(path: string, opts: {
  method?: string; body?: unknown; signal?: AbortSignal; revision?: number; idempotencyKey?: string; desktop?: boolean
} = {}): Promise<T> {
  const method = opts.method || 'GET'
  const headers = new Headers({ Accept: 'application/json' })
  if (method !== 'GET' && account.session.csrf_token) headers.set('x-csrf-token', account.session.csrf_token)
  if (opts.body !== undefined) headers.set('content-type', 'application/json')
  if (opts.desktop) headers.set('x-gallery-client', 'desktop')
  if (opts.idempotencyKey) headers.set('idempotency-key', opts.idempotencyKey)
  if (opts.revision !== undefined) headers.set('if-match', `"${opts.revision}"`)
  const response = await fetch(`/api/v1${path}`, {
    credentials: 'same-origin', cache: 'no-store', method, headers, signal: opts.signal,
    body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
  })
  if (!response.ok) {
    const problem = await response.json().catch(() => ({})) as Problem
    if (response.status === 401 && path !== '/session' && account.session.actor) account.session = { actor: null }
    throw new ApiError(response.status, problem)
  }
  if (response.status === 204) return undefined as T
  return await response.json() as T
}

export async function loadAccount() {
  const [session, setup, capabilities, presentation] = await Promise.all([
    request<Session>('/session'),
    request<{ initialized: boolean }>('/setup/status'),
    request<Capabilities>('/capabilities'),
    request<Presentation>('/public/profile'),
  ])
  account.session = session
  account.initialized = setup.initialized
  account.capabilities = capabilities
  account.presentation = presentation
}

export function statusText(state?: string) {
  const labels: Record<string, string> = {
    starting: '正在启动', completed: '已完成', dispatching: '正在交付', delivery_unknown: '交付待核对', interrupted: '已停止',
    waiting: '等待上传', receiving: '正在上传', validating: '正在校验', committed: '已入册',
    queued: '等待执行', building: '正在打包', running: '处理中', retry_wait: '等待重试',
    cancel_requested: '正在取消', canceled: '已取消', succeeded: '已完成', complete: '已完成',
    failed: '失败', ready: '可下载', expired: '已过期', stale: '已失效', partial: '部分完成',
    active: '进行中', idle: '空闲', archived: '已归档', unavailable: '不可用',
    stop_requested: '正在停止', needs_reconcile: '待核对', protected: '保护期',
    verified: '已验证', proposed: '待确认', approved: '已确认', rejected: '已拒绝',
  }
  return state ? labels[state] || state : ''
}

export function errorText(error: unknown): string {
  if (error instanceof ApiError) {
    const specific: Record<string, string> = {
      NOT_INITIALIZED: '请先设置图库管理员。',
      UNAUTHENTICATED: '请登录后继续。',
      MUST_CHANGE_PASSWORD: '请先更改密码。',
      REVISION_CONFLICT: '内容有更新。请刷新后比较并再次保存。',
      CURSOR_STALE: '列表内容已变更，请刷新后继续。',
      RATE_LIMIT: '请求过于频繁，请稍后重试。',
      FEATURE_GATED: '当前实例未启用该功能。',
      INVALID_CREDENTIALS: '账号或密码有误。',
      DOWNLOAD_DISABLED: '相册已关闭下载。',
    }
    if (specific[error.problem.code || '']) return specific[error.problem.code || '']
    if (error.status === 401) return '账号或密码有误，或登录已过期。'
    if (error.status === 403) return '当前账号没有权限执行此操作。'
    if (error.status === 404) return '该内容不存在或当前账号无法访问。'
    return `${error.message}${error.problem.request_id ? `（请求 ${error.problem.request_id}）` : ''}`
  }
  if (error instanceof TypeError) return '无法连接到 Gallery，请检查网络后重试。'
  return error instanceof Error ? error.message : '操作失败，请重试。'
}
