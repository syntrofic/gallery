import { mediaPath, type Photo, type Variant } from './api'

function ratio(variant: Variant) {
  return variant.width && variant.height ? variant.width / variant.height : 0
}

export function photoAspectRatio(photo: Photo) {
  const original = Math.max(1, photo.width) / Math.max(1, photo.height)
  // JPEG previews are already rotated for EXIF orientation.
  const previews = (photo.variants || []).filter(variant => variant.spec_id !== 'thumb').map(ratio)
  if (previews.some(value => Math.abs(value / original - 1) < .04)) return original
  if (previews.some(value => Math.abs(value * original - 1) < .04)) return 1 / original
  return original
}

export function fittedVariants(variants: Variant[], aspect: number) {
  return variants.filter(variant => Math.abs(ratio(variant) / aspect - 1) < .04)
}

export function mediaSrcset(variants: Variant[], album: string, image: string) {
  const widths = new Map<number, string>()
  for (const variant of variants) {
    if (variant.width && !widths.has(variant.width)) widths.set(variant.width, mediaPath(album, image, variant.id))
  }
  if (widths.size > 1) {
    return [...widths].sort(([a], [b]) => a - b).map(([width, url]) => `${url} ${width}w`).join(', ')
  }
}
