import { cn } from "@/lib/cn";

export interface LogoProps {
  size?: number;
  className?: string;
  /** Accessible name; when omitted the logo is decorative. */
  title?: string;
}

/**
 * Nuncio's brand mark: an envelope with a postmark ring and a notification dot.
 *
 * The small sizes are NOT downscaled copies of the large mark. Below roughly
 * 32px the master's strokes collide, the postmark ring closes up and the dot
 * merges into the envelope, so the design ships a separate optical master per
 * size, drawn on its own grid with adjusted strokes. Picking the matching one
 * for the slot keeps the mark legible where a scaled master turns to mush.
 *
 * Above 32px the master art is used as drawn: it is vector, so it stays crisp
 * at any size (the 64px first-run screen, and any larger use later).
 *
 * The shapes are inlined rather than imported as files because the project has
 * no SVG loader configured, and adding one for a single mark would pull a build
 * dependency in for no gain.
 */
const OPTICAL_MASTERS: Record<number, string> = {
  16: `<rect width="16" height="16" rx="3.6266666675" fill="#F6F0E8"/><path d="M2.75 6.25H13.25Q14.25 6.25 14.25 7.25V13.0Q14.25 14.0 13.25 14.0H2.75Q1.75 14.0 1.75 13.0V7.25Q1.75 6.25 2.75 6.25Z" fill="#384F53"/><path d="M3.0 7.8L8.0 11.442499999999999L13.0 7.8" fill="none" stroke="#E2E4D7" stroke-width="1.05" stroke-linecap="round" stroke-linejoin="round"/><path d="M2.4 3.2C3.525 2.45 4.275 3.95 5.525 3.2S7.4 2.45 8.65 3.2" fill="none" stroke="#806951" stroke-width="1" stroke-linecap="round" stroke-linejoin="round"/><circle cx="11.5" cy="5" r="4.325" fill="#F6F0E8"/><circle cx="11.5" cy="5" r="3.15" fill="none" stroke="#806951" stroke-width="1.05"/><path d="M10.05 6.75V3.25L12.950000000000001 6.75V3.25" fill="none" stroke="#685347" stroke-width="1.05" stroke-linecap="round" stroke-linejoin="round"/>`,
  18: `<rect width="18" height="18" rx="4.0800000009375" fill="#F6F0E8"/><path d="M3.125 7H14.875Q16 7 16 8.125V14.625Q16 15.75 14.875 15.75H3.125Q2 15.75 2 14.625V8.125Q2 7 3.125 7Z" fill="#384F53"/><path d="M3.40625 8.74375L9.0 12.8625L14.59375 8.74375" fill="none" stroke="#E2E4D7" stroke-width="1.05" stroke-linecap="round" stroke-linejoin="round"/><path d="M2.6999999999999997 3.6C3.933 2.75625 4.755 4.44375 6.125 3.6S8.180000000000001 2.75625 9.55 3.6" fill="none" stroke="#806951" stroke-width="1" stroke-linecap="round" stroke-linejoin="round"/><circle cx="12.75" cy="5.6" r="4.7562500000000005" fill="#F6F0E8"/><circle cx="12.75" cy="5.6" r="3.5" fill="none" stroke="#806951" stroke-width="1.05"/><path d="M11.15 7.6V3.5999999999999996L14.350000000000001 7.6V3.5999999999999996" fill="none" stroke="#685347" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round"/>`,
  20: `<rect width="20" height="20" rx="4.5333333343749995" fill="#F6F0E8"/><path d="M3.5 7.75H16.5Q17.75 7.75 17.75 9.0V16.25Q17.75 17.5 16.5 17.5H3.5Q2.25 17.5 2.25 16.25V9.0Q2.25 7.75 3.5 7.75Z" fill="#384F53"/><path d="M3.8125 9.6875L10.0 14.2825L16.1875 9.6875" fill="none" stroke="#E2E4D7" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round"/><path d="M3.0 4.0C4.377 3.0625 5.295 4.9375 6.825 4.0S9.120000000000001 3.0625 10.65 4.0" fill="none" stroke="#806951" stroke-width="1.05" stroke-linecap="round" stroke-linejoin="round"/><circle cx="14.25" cy="6.1" r="5.2625" fill="#F6F0E8"/><circle cx="14.25" cy="6.1" r="3.9" fill="none" stroke="#806951" stroke-width="1.1"/><path d="M12.45 8.35V3.8499999999999996L16.05 8.35V3.8499999999999996" fill="none" stroke="#685347" stroke-width="1.15" stroke-linecap="round" stroke-linejoin="round"/>`,
  24: `<rect width="24" height="24" rx="5.44000000125" fill="#F6F0E8"/><path d="M4.25 9.25H19.75Q21.25 9.25 21.25 10.75V19.25Q21.25 20.75 19.75 20.75H4.25Q2.75 20.75 2.75 19.25V10.75Q2.75 9.25 4.25 9.25Z" fill="#384F53"/><path d="M4.625 11.575L12.0 16.955L19.375 11.575" fill="none" stroke="#E2E4D7" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/><path d="M3.5999999999999996 4.800000000000001C5.292 3.6750000000000007 6.42 5.925000000000001 8.3 4.800000000000001S11.120000000000001 3.6750000000000007 13.0 4.800000000000001" fill="none" stroke="#806951" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round"/><circle cx="17.25" cy="7.25" r="6.125" fill="#F6F0E8"/><circle cx="17.25" cy="7.25" r="4.55" fill="none" stroke="#806951" stroke-width="1.2"/><path d="M15.05 9.95V4.55L19.450000000000003 9.95V4.55" fill="none" stroke="#685347" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>`,
  32: `<rect width="32" height="32" rx="7.253333335" fill="#F6F0E8"/><path d="M6.5 12H25.5Q27.5 12 27.5 14.0V23.5Q27.5 25.5 25.5 25.5H6.5Q4.5 25.5 4.5 23.5V14.0Q4.5 12 6.5 12Z" fill="#384F53"/><path d="M7.0 15.1L16.0 21.045L25.0 15.1" fill="none" stroke="#E2E4D7" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/><path d="M4.8 6.4C6.851999999999999 4.9 8.219999999999999 7.9 10.5 6.4S13.919999999999998 4.9 16.2 6.4M4.8 3.7C6.851999999999999 2.2 8.219999999999999 5.2 10.5 3.7S13.919999999999998 2.2 16.2 3.7" fill="none" stroke="#806951" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round"/><circle cx="22" cy="10" r="7.999999999999999" fill="#F6F0E8"/><circle cx="22" cy="10" r="6.1" fill="none" stroke="#806951" stroke-width="1.2"/><path d="M19.15 13.5V6.5L24.849999999999998 13.5V6.5" fill="none" stroke="#685347" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>`,
};

const MASTER_ART = `<path xmlns="http://www.w3.org/2000/svg" d="M13 24H51Q55 24 55 28V47Q55 51 51 51H13Q9 51 9 47V28Q9 24 13 24Z" fill="#384F53" />
<path xmlns="http://www.w3.org/2000/svg" d="M13 29L32 43L51 29" fill="none" stroke="#E2E4D7" stroke-width="2.8" stroke-linecap="round" stroke-linejoin="round" />
<path xmlns="http://www.w3.org/2000/svg" d="M10 12C14 9 17 15 22.5 12C26.5 9 30 15 35 12" fill="none" stroke="#9B8268" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" />
<path xmlns="http://www.w3.org/2000/svg" d="M10 17C14 14 17 20 22.5 17C26.5 14 30 20 35 17" fill="none" stroke="#9B8268" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" />
<circle xmlns="http://www.w3.org/2000/svg" cx="44" cy="20" r="13.25" fill="#F6F0E8" />
<circle xmlns="http://www.w3.org/2000/svg" cx="44" cy="20" r="11.2" fill="none" stroke="#9B8268" stroke-width="1.6" />
<path xmlns="http://www.w3.org/2000/svg" d="M39.1 25.6V14.4L48.900000000000006 25.6V14.4" fill="none" stroke="#685347" stroke-width="2.7" stroke-linecap="round" stroke-linejoin="round" />`;

/** Sizes that have a purpose-drawn master, ascending. */
const OPTICAL_SIZES = [16, 18, 20, 24, 32];

interface Chosen {
  /** SVG source for the shapes. */
  art: string;
  /** The coordinate system `art` is drawn in. */
  box: number;
}

function chooseMark(size: number): Chosen {
  // Round up: a 19px slot is better served by the 20px master than the 18px
  // one, since an under-sized master reads thin at the size it is drawn at.
  const match = OPTICAL_SIZES.find((candidate) => candidate >= size);
  if (match === undefined) return { art: MASTER_ART, box: 64 };
  return { art: OPTICAL_MASTERS[match] ?? MASTER_ART, box: match };
}

export function Logo({ size = 24, className, title }: LogoProps) {
  const { art, box } = chooseMark(size);
  return (
    <svg
      width={size}
      height={size}
      viewBox={`0 0 ${box} ${box}`}
      role={title ? "img" : undefined}
      aria-label={title}
      aria-hidden={title ? undefined : true}
      className={cn(className)}
      focusable="false"
      // The art comes from the design handoff, not from user input or IPC.
      // React cannot render SVG presentation attributes from a string as
      // children, and parsing the mark per render would be wasteful.
      dangerouslySetInnerHTML={{ __html: art }}
    />
  );
}
