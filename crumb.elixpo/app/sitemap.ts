import type { MetadataRoute } from 'next'

export default function sitemap(): MetadataRoute.Sitemap {
  return ['', '/about', '/docs', '/privacy', '/terms'].map(path => ({ url: `https://crumb.elixpo.com${path}`, changeFrequency: path ? 'monthly' : 'weekly', priority: path ? 0.7 : 1 }))
}
