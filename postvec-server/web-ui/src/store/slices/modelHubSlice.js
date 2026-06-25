const createModelHubSlice = () => ({
  modelhub: {
    filesystem: {
      local: {
        models: [],
        loading: false,
        selected_model: '',
      },
    },
    prefs: {
      filter: '',
      refreshing: false,
      active_remote_alias: 'local',
    },
  },
})

export default createModelHubSlice
