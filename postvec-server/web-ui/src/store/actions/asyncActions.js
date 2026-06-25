import axios from 'axios'

export const createAsyncActions = (set, get) => ({
  appMounted: async () => {
    set((draft) => {
      draft.prefs.loading = true
    })
    try {
      await get().getConfiguration()
    } catch (error) {
      console.error('Error fetching initial data:', error)
    } finally {
      set((draft) => {
        draft.prefs.loading = false
      })
    }
  },

  getConfiguration: async () => {
    const state = get()
    const url = state.api_endpoint + '/config'
    const { data: response } = await axios.get(url)

    set((draft) => {
      if (response && response.success === true) {
        const hubModels = response.data?.models || []
        const cluster = response.data?.cluster || {}
        const clusterNodes = cluster.nodes || []
        const currentSelectedModelName =
          state.modelhub?.filesystem?.local?.selected_model || ''
        draft.modelhub.prefs.active_remote_alias = 'local'
        draft.modelhub.filesystem.local = {
          models: hubModels,
          loading: false,
          selected_model: currentSelectedModelName,
        }
        draft.cluster = clusterNodes
        draft.system = response.data?.system || null
        draft.server = response.data?.server || null
      }
    })

    return response
  },

  executeQuery: async (model, peers, query) => {
    if (!model) {
      throw new Error('Cannot execute query - no model specified')
    }
    if (!peers || !Array.isArray(peers) || peers.length === 0) {
      throw new Error('Cannot execute query - no peers specified')
    }
    if (!query) {
      throw new Error('Cannot execute query - no query specified')
    }

    const modelName = model.name
    set((draft) => {
      const currentModelCache = draft.prefs.query_cache.models[modelName] || {}
      currentModelCache.loading = true
      currentModelCache.responses = []
      currentModelCache.performed = false
      draft.prefs.query_cache.models[modelName] = currentModelCache
    })

    try {
      const liftFuture = async (peer, name, body) => {
        try {
          const url = `${peer}/api/${name}`
          const data = JSON.parse(body)
          const { data: response } = await axios.post(url, data)
          if (response.success) {
            return { peer, model: name, success: true, result: response.data }
          }
          const errorMessage = response?.error?.message || 'query failed'
          return {
            peer,
            model: name,
            success: false,
            error: new Error(`Query failed (${name}) from node ${peer}: ${errorMessage}`),
          }
        } catch (err) {
          return { success: false, error: err, peer, model: name }
        }
      }

      const responses = await Promise.all(
        peers.map((peer) => liftFuture(peer, modelName, query)),
      )

      set((draft) => {
        const currentModelCache = draft.prefs.query_cache.models[modelName] || {}
        currentModelCache.loading = false
        currentModelCache.performed = true
        currentModelCache.responses = responses
        draft.prefs.query_cache.models[modelName] = currentModelCache
      })

      return responses
    } catch (error) {
      set((draft) => {
        const currentModelCache = draft.prefs.query_cache.models[modelName] || {}
        currentModelCache.loading = false
        currentModelCache.performed = true
        currentModelCache.responses = []
        draft.prefs.query_cache.models[modelName] = currentModelCache
      })
      throw error
    }
  },
})
