// The node that served the page is the default query target. Not persisted:
// the same build may be served by another node tomorrow. Under `npm run dev`
// the Vite proxy forwards /config and /api to VITE_API_ENDPOINT.
const createNetworkingSlice = () => ({
  api_endpoint: "https://127.0.0.1:22222",
  api_endpoint2: window.location.origin,
  cluster: [],
  system: null,
  server: null,
  loading: false,
  error: null,
})

export default createNetworkingSlice
