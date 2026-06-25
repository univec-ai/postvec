import { getClusterMembers } from '../selectors/clusterSelectors'

const createPrefsSlice = (set, get) => ({
  prefs: {
    current_model_name: '',
    filter: '',
    query_peers: [],
    main_menu_tab: 'queries',
    loading: false,
    query_cache: {
      models: {},
      inputs: {},
    },
  },

  setActiveModel: (modelName) =>
    set((state) => {
      state.prefs.current_model_name = modelName
    }),

  setMainMenuTab: (tab) =>
    set((state) => {
      state.prefs.main_menu_tab = tab
    }),

  setFilter: (filter) =>
    set((state) => {
      state.prefs.filter = filter
    }),

  toggleQueryPeer: (peer, active) =>
    set((state) => {
      if (!state.prefs.query_peers) {
        state.prefs.query_peers = []
      }
      let currentPeers = state.prefs.query_peers || []
      if (active) {
        if (!currentPeers.includes(peer)) {
          currentPeers.push(peer)
        }
      } else {
        currentPeers = currentPeers.filter((p) => p !== peer)
      }
      const currentMembers = getClusterMembers(state).reduce((acc, curr) => {
        acc[curr] = true
        return acc
      }, {})
      currentPeers = currentPeers.filter((p) => currentMembers.hasOwnProperty(p))
      state.prefs.query_peers = currentPeers
    }),

  cacheQuery: (query) =>
    set((state) => {
      const currentModel = get().getCurrentModel()
      if (!currentModel) {
        return
      }
      const currentModelCache = state.prefs.query_cache.models[currentModel.name] || {}
      if (currentModelCache.text === query) {
        return
      }
      currentModelCache.text = query
      currentModelCache.loading = false
      currentModelCache.performed = false
      try {
        const code = JSON.parse(query)
        const extractAllValues = (obj) => {
          const out = {}
          if (typeof obj === 'object' && obj !== null) {
            for (const key of Object.keys(obj)) {
              if (typeof obj[key] === 'object' && !(obj[key] instanceof Array)) {
                const values = extractAllValues(obj[key])
                for (const newKey of Object.keys(values)) {
                  out[newKey] = values[newKey]
                }
              }
              out[key] = obj[key]
            }
          }
          return out
        }
        const allValuesForInputs = extractAllValues(code)
        currentModelCache.inputs = allValuesForInputs
        const globalInputsCache = state.prefs.query_cache.inputs
        for (const key of Object.keys(allValuesForInputs)) {
          globalInputsCache[key] = allValuesForInputs[key]
        }
        currentModelCache.responses = []
        state.prefs.query_cache.inputs = globalInputsCache
      } catch {
        // Invalid JSON is stored as text only; the editor flags it.
      }
      state.prefs.query_cache.models[currentModel.name] = currentModelCache
    }),
})

export default createPrefsSlice
