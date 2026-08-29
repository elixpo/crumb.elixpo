import type { Metadata } from 'next'
import './globals.css'

export const metadata: Metadata = {
  title: 'Crumb — your terminal, intelligently layered',
  description: 'A native shell with an optional intelligence layer.',
}

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return <html lang="en"><body>{children}</body></html>
}
