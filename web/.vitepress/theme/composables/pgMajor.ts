import { onMounted, ref, watch, type Ref } from "vue";

export type PgMajor = 16 | 17 | 18;

export const PG_MAJORS: readonly PgMajor[] = [16, 17, 18];

const STORAGE_KEY = "postvec:pg-major";
const FAMILY_KEY = "postvec:pkg-family";

export type PkgFamily = "debian" | "el9";

const pgMajor = ref<PgMajor>(18);
const pkgFamily = ref<PkgFamily>("debian");
let started = false;

function readMajor(): PgMajor {
  try {
    const n = Number(localStorage.getItem(STORAGE_KEY));
    if (n === 16 || n === 17 || n === 18) return n;
  } catch {
    /* private mode / SSR */
  }
  return 18;
}

function readFamily(): PkgFamily {
  try {
    const v = localStorage.getItem(FAMILY_KEY);
    if (v === "debian" || v === "el9") return v;
  } catch {
    /* private mode / SSR */
  }
  return "debian";
}

/** Shared across every snippet on the site. Hydrates from localStorage after mount. */
export function usePgMajor(): Ref<PgMajor> {
  ensureStarted();
  return pgMajor;
}

export function usePkgFamily(): Ref<PkgFamily> {
  ensureStarted();
  return pkgFamily;
}

function ensureStarted() {
  if (started || typeof window === "undefined") return;
  started = true;
  onMounted(() => {
    pgMajor.value = readMajor();
    pkgFamily.value = readFamily();
    watch(pgMajor, (v) => {
      try {
        localStorage.setItem(STORAGE_KEY, String(v));
      } catch {
        /* ignore */
      }
    });
    watch(pkgFamily, (v) => {
      try {
        localStorage.setItem(FAMILY_KEY, v);
      } catch {
        /* ignore */
      }
    });
  });
}
