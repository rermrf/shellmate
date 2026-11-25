//! FIFO identity routing for independent main/child approval reply owners.
pub struct Queue<T>(Vec<(String, Option<String>, T)>);
impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}
impl<T> Queue<T> {
    pub fn insert(&mut self, key: String, source: Option<String>, value: T) {
        if let Some(old) = self.0.iter_mut().find(|e| e.0 == key) {
            old.2 = value;
        } else {
            self.0.push((key, source, value));
        }
    }
    pub fn get(&self, key: &str) -> Option<&T> {
        self.0.iter().find(|e| e.0 == key).map(|e| &e.2)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut T> {
        self.0.iter_mut().find(|e| e.0 == key).map(|e| &mut e.2)
    }
    pub fn remove(&mut self, key: &str) -> Option<T> {
        let i = self.0.iter().position(|e| e.0 == key)?;
        Some(self.0.remove(i).2)
    }
    pub fn remove_source(&mut self, source: &str) {
        self.0.retain(|e| e.1.as_deref() != Some(source));
    }
    pub fn clear_main(&mut self) {
        self.0.retain(|e| e.1.is_some());
    }
    pub fn take_main(&mut self) -> Option<T> {
        let i = self.0.iter().position(|e| e.1.is_none())?;
        Some(self.0.remove(i).2)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &T)> {
        self.0.iter().map(|e| (e.0.as_str(), &e.2))
    }
}
