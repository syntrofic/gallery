const imageExtensions: Record<string, string> = {
  'image/avif': 'avif', 'image/jpeg': 'jpg', 'image/png': 'png', 'image/webp': 'webp',
}

export async function downloadPhotoPreview(url: string, name: string, signal: AbortSignal) {
  const response = await fetch(url, { credentials: 'same-origin', signal })
  const extension = imageExtensions[response.headers.get('content-type')?.split(';')[0]?.trim().toLowerCase() ?? '']
  if (!response.ok || !extension) throw new Error('Invalid photo response')
  const data = await response.blob()
  if (!data.size || signal.aborted) throw new Error('Photo download interrupted')
  const filename = name.split(/[\\/]/).pop()?.replace(/\.[^.]+$/, '').replace(/[<>:"|?*\u0000-\u001f]/g, '_').trim()
  const objectUrl = URL.createObjectURL(data)
  const anchor = document.createElement('a')
  anchor.href = objectUrl
  anchor.download = `${filename || 'photo'}.${extension}`
  anchor.hidden = true
  document.body.append(anchor)
  anchor.click()
  anchor.remove()
  window.setTimeout(() => URL.revokeObjectURL(objectUrl), 30_000)
}
