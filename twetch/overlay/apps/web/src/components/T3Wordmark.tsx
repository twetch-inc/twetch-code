import type { SVGProps } from "react";

/** TwetchCode's own mark in place of the upstream wordmark. In a square box it is the prompt mark. */
export function T3Wordmark(props: SVGProps<SVGSVGElement>) {
  if (/\bsize-/.test(props.className ?? "")) {
    return (
      <svg {...props} viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        <path
          d="M5 6l6 6-6 6"
          fill="none"
          stroke="currentColor"
          strokeWidth="2.6"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
        <circle cx="17.5" cy="17" r="2.4" fill="currentColor" />
      </svg>
    );
  }
  return (
    <svg {...props} viewBox="0 0 45 10" xmlns="http://www.w3.org/2000/svg">
      <text
        x="0"
        y="10"
        fill="currentColor"
        fontFamily="inherit"
        fontSize="13.9"
        fontWeight="500"
        textLength="45"
        lengthAdjust="spacingAndGlyphs"
      >
        Twetch
      </text>
    </svg>
  );
}
