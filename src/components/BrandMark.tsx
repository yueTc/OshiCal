export default function BrandMark({ size = 24 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 64 64"
      fill="none"
      aria-hidden="true"
      focusable="false"
    >
      <rect x="10" y="15" width="44" height="41" rx="10" stroke="currentColor" strokeWidth="4.5" />
      <path d="M10 25H54" stroke="currentColor" strokeWidth="4.5" />
      <path d="M20 9V18M44 9V18" stroke="currentColor" strokeWidth="4.5" strokeLinecap="round" />
      <path d="M27 34.5L42 40L27 45.5V34.5Z" fill="currentColor" />
      <circle cx="51" cy="13" r="7" fill="#16A46B" stroke="#FFF8FB" strokeWidth="2.6" />
    </svg>
  );
}
