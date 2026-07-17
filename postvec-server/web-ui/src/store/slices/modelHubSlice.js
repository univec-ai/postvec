// The registry key lives in this tab only: sent as a bearer on catalogue
// and pull requests, never stored on the node, gone when the tab closes.
const KEY = 'postvec-registry-key'
const savedKey = () => {
  try {
    return sessionStorage.getItem(KEY) || ''
  } catch {
    return ''
  }
}

const createModelHubSlice = (set) => ({
  models: [],
  setRegistryKey: (key) =>
    set((draft) => {
      draft.registry.key = key.trim()
      try {
        if (draft.registry.key) sessionStorage.setItem(KEY, draft.registry.key)
        else sessionStorage.removeItem(KEY)
      } catch {
        // No session storage (private mode): the key still works for this page load.
      }
    }),
  registry: {
    key: savedKey(),
    installed: [],
    available: [],
    channel: null,
    authenticated: false,
    signed_in_as: null,
    pulls: [],
    loading: false,
    error: null,
    busy: null,
  },
})

export default createModelHubSlice
