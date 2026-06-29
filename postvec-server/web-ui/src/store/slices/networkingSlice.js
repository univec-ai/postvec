// The node that served the page is the default query target. Not persisted:
// the same build may be served by another node tomorrow.
const createNetworkingSlice = () => ({
  api_endpoint: "https://127.0.0.1:22222",
  api_endpoint2:
    typeof window !== 'undefined' && window.location.protocol.startsWith('http')
      ? window.location.origin
      : 'http://127.0.0.1:22222',
  cluster: [],
  system: null,
  server: null,
  loading: false,
  error: null,
})

export default createNetworkingSlice
