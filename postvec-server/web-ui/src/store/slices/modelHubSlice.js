const createModelHubSlice = () => ({
  models: [],
  registry: {
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
