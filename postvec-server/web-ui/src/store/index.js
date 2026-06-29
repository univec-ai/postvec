import { create } from 'zustand'
import { devtools, persist } from 'zustand/middleware'
import { immer } from 'zustand/middleware/immer'
import createModelHubSlice from './slices/modelHubSlice'
import createPrefsSlice from './slices/prefsSlice'
import createNetworkingSlice from './slices/networkingSlice'
import { createAsyncActions } from './actions/asyncActions'

// Persisted: the selection and what was typed, never responses (an
// embedding batch would blow the localStorage quota) and never the endpoint.
const persisted = (state) => ({
  prefs: {
    ...state.prefs,
    query_peers: [],
    query_cache: {
      models: Object.fromEntries(
        Object.entries(state.prefs.query_cache.models).map(([name, cache]) => [
          name,
          { contract: cache.contract, text: cache.text, responses: [] },
        ]),
      ),
    },
  },
})

export const useStore = create(
  devtools(
    persist(
      immer((set, get) => ({
        ...createModelHubSlice(set, get),
        ...createPrefsSlice(set, get),
        ...createNetworkingSlice(set, get),
        ...createAsyncActions(set, get),
      })),
      {
        name: 'postvec-server-storage',
        version: 1,
        migrate: () => ({}),
        partialize: persisted,
        merge: (saved, current) => ({
          ...current,
          prefs: {
            ...current.prefs,
            ...saved?.prefs,
            query_cache: { models: saved?.prefs?.query_cache?.models || {} },
          },
        }),
      },
    ),
    { name: 'PostvecServerStore' },
  ),
)

export * from './selectors/modelSelectors'
export * from './selectors/clusterSelectors'
export { requestPath } from './actions/asyncActions'
