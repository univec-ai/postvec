export const modelCache = (state, name) => {
  state.prefs.query_cache.models[name] ||= { contract: 'native', text: {}, responses: [] }
  return state.prefs.query_cache.models[name]
}

const createPrefsSlice = (set) => ({
  prefs: {
    current_model_name: '',
    filter: '',
    query_peers: [],
    main_menu_tab: 'queries',
    // Per model: { contract, text: { native, openai }, responses, loading }.
    query_cache: { models: {} },
  },

  setActiveModel: (name) =>
    set((state) => {
      state.prefs.current_model_name = name
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
      const peers = state.prefs.query_peers.filter((p) => p !== peer)
      if (active) peers.push(peer)
      state.prefs.query_peers = peers
    }),

  setContract: (name, contract) =>
    set((state) => {
      const cache = modelCache(state, name)
      cache.contract = contract
      cache.responses = []
    }),

  resetQuery: (name, contract) =>
    set((state) => {
      const cache = modelCache(state, name)
      delete cache.text[contract]
      cache.responses = []
    }),

  cacheQuery: (name, contract, text) =>
    set((state) => {
      const cache = modelCache(state, name)
      if (cache.text[contract] === text) return
      cache.text[contract] = text
      cache.responses = []
    }),
})

export default createPrefsSlice
