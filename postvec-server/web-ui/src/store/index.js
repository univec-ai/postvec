import { create } from 'zustand'
import { devtools, persist } from 'zustand/middleware'
import { immer } from 'zustand/middleware/immer'
import createModelHubSlice from './slices/modelHubSlice'
import createPrefsSlice from './slices/prefsSlice'
import createNetworkingSlice from './slices/networkingSlice'
import { createAsyncActions } from './actions/asyncActions'
import { getCurrentModel } from './selectors/modelSelectors'

export const useStore = create(
  devtools(
    persist(
      immer((set, get) => ({
        ...createModelHubSlice(set, get),
        ...createPrefsSlice(set, get),
        ...createNetworkingSlice(set, get),
        getCurrentModel: () => getCurrentModel(get()),
        ...createAsyncActions(set, get),
      })),
      {
        name: 'postvec-server-storage',
        partialize: (state) => ({
          api_endpoint: state.api_endpoint || '',
          prefs: {
            current_model_name: state.prefs?.current_model_name || '',
            filter: state.prefs?.filter || '',
            main_menu_tab: state.prefs?.main_menu_tab || 'queries',
            query_cache: {
              models: state.prefs?.query_cache?.models || {},
              inputs: state.prefs?.query_cache?.inputs || {},
            },
          },
        }),
      },
    ),
    { name: 'PostvecServerStore' },
  ),
)

export * from './selectors/modelSelectors'
export * from './selectors/clusterSelectors'
