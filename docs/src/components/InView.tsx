import React, { useEffect, useRef, useState } from "react";

/** Tracks whether an element is in the viewport, so diagrams animate on scroll. */
export function useInView(
  threshold = 0.25,
): [React.RefObject<HTMLElement | null>, boolean] {
  const ref = useRef<HTMLElement | null>(null);
  const [inView, setInView] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const obs = new IntersectionObserver(
      ([entry]) => setInView(entry.isIntersecting),
      { threshold },
    );
    obs.observe(el);
    return () => obs.disconnect();
  }, [threshold]);
  return [ref, inView];
}

/**
 * Renders `children` with a flag that becomes true once the wrapper scrolls
 * into view. Used in MDX to drive the animated figures:
 *
 * ```mdx
 * <InView>{(active) => <MemoryChart active={active} />}</InView>
 * ```
 */
export default function InView({
  threshold,
  children,
}: {
  threshold?: number;
  children: (active: boolean) => React.ReactNode;
}): React.JSX.Element {
  const [ref, active] = useInView(threshold);
  return (
    <div ref={ref as React.RefObject<HTMLDivElement | null>}>
      {children(active)}
    </div>
  );
}
