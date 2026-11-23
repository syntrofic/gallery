import type { RouteLocationNormalizedLoaded } from 'vue-router'

export interface GalleryPhotoRef { albumId: string; imageId: string }

export function galleryPhoto(route: RouteLocationNormalizedLoaded, albumId?: string): GalleryPhotoRef | undefined {
  const image = route.query.photo
  const album = albumId || route.query.photoAlbum
  if (typeof image === 'string' && image && typeof album === 'string' && album) {
    return { albumId: album, imageId: image }
  }
}

export function galleryPhotoLink(route: RouteLocationNormalizedLoaded, albumId: string, imageId: string) {
  return {
    path: route.path,
    query: { ...route.query, photo: imageId, photoAlbum: route.params.albumId ? undefined : albumId },
    state: { galleryLightbox: true },
  }
}
