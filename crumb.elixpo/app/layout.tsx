import type { Metadata } from 'next'
import { Geist, Geist_Mono } from 'next/font/google'
import './globals.css'

const sans = Geist({ subsets: ['latin'], variable: '--font-sans' })
const mono = Geist_Mono({ subsets: ['latin'], variable: '--font-mono' })
const siteUrl = 'https://crumb.elixpo.com'
const title = 'Crumb — The Natural Language Terminal'
const description = 'A native-first terminal that runs shell commands normally and brings in models, skills, and tools only when you ask.'

export const metadata: Metadata = {
  metadataBase: new URL(siteUrl),
  title: { default: title, template: '%s · Crumb' },
  description,
  applicationName: 'Crumb NLT',
  keywords: ['natural language terminal', 'AI terminal', 'shell', 'developer tools', 'Crumb'],
  authors: [{ name: 'Elixpo', url: 'https://elixpo.com' }],
  creator: 'Elixpo',
  publisher: 'Elixpo',
  alternates: { canonical: '/' },
  manifest: '/manifest.webmanifest',
  robots: { index: true, follow: true },
  openGraph: {
    type: 'website',
    url: siteUrl,
    siteName: 'Crumb NLT',
    title,
    description,
    images: [{ url: '/og-image.png', width: 1280, height: 720, alt: 'Crumb — the terminal that speaks your native language' }],
  },
  twitter: { card: 'summary_large_image', title, description, images: ['/og-image.png'] },
  icons: {
    icon: [{ url: '/favicon.ico' }, { url: '/favicon.png', type: 'image/png', sizes: '1024x1024' }],
    shortcut: '/favicon.ico',
    apple: '/apple-touch-icon.png',
  },
  formatDetection: { telephone: false },
}

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  const software = {
    '@context': 'https://schema.org', '@type': 'SoftwareApplication', name: 'Crumb',
    alternateName: 'Crumb NLT', applicationCategory: 'DeveloperApplication', operatingSystem: 'Linux, macOS, Windows',
    description, url: siteUrl, codeRepository: 'https://github.com/elixpo/crumb.elixpo',
    downloadUrl: 'https://github.com/elixpo/crumb.elixpo/releases',
    offers: { '@type': 'Offer', price: '0', priceCurrency: 'USD' },
  }
  return <html lang="en" className={`${sans.variable} ${mono.variable}`}>
    <body>{children}<script type="application/ld+json" dangerouslySetInnerHTML={{ __html: JSON.stringify(software) }} /></body>
  </html>
}
