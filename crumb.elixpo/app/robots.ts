import type { MetadataRoute } from 'next'

export default function robots(): MetadataRoute.Robots {
  return { rules: { userAgent: '*', allow: '/', disallow: ['/api/', '/connect'] }, sitemap: 'https://crumb.elixpo.com/sitemap.xml' }
}
