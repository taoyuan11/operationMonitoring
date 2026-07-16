import { createApp } from 'vue'
import './style.css'
import App from './App.vue'
import { bootstrapCachedAppearance } from './utils/appearance'

bootstrapCachedAppearance()
createApp(App).mount('#app')
