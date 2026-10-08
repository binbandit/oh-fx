use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

#[derive(Debug, Clone, Default)]
pub struct LiveAdditionalRoots(Arc<RwLock<Arc<[PathBuf]>>>);

impl LiveAdditionalRoots {
    pub fn get(&self) -> Arc<[PathBuf]> {
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn set(&self, roots: Vec<PathBuf>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = roots.into();
    }
}

impl From<Vec<PathBuf>> for LiveAdditionalRoots {
    fn from(roots: Vec<PathBuf>) -> Self {
        Self(Arc::new(RwLock::new(roots.into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_clone_sees_the_roots_set_through_any_of_them() {
        let roots = LiveAdditionalRoots::from(vec![PathBuf::from("/srv/shared")]);
        let shared = roots.clone();
        assert_eq!(*shared.get(), [PathBuf::from("/srv/shared")]);
        roots.set(vec![PathBuf::from("/srv/docs"), PathBuf::from("/srv/data")]);
        assert_eq!(
            *shared.get(),
            [PathBuf::from("/srv/docs"), PathBuf::from("/srv/data")]
        );
        assert!(LiveAdditionalRoots::default().get().is_empty());
    }
}
