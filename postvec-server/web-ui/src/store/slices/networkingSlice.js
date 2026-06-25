const defaultEndpoint =
  typeof window !== 'undefined' && window.location.protocol.startsWith('http')
    ? window.location.origin
    : 'http://127.0.0.1:22222'

const createNetworkingSlice = () => ({
  api_endpoint: defaultEndpoint,
  cluster: [],
  system: null,
  server: null,
})

export default createNetworkingSlice
