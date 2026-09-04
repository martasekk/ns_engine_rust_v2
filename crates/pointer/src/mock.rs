//! A pointer that records instead of moving. Every rule in this crate is
//! proven against it, which is the point: the machine this was written on has
//! no display server at all.

use crate::geom::{Point, Screens};
use crate::wire::{InputError, Step};
use crate::Pointer;
use async_trait::async_trait;
use std::sync::Mutex;

pub struct MockPointer {
    pub screens: Screens,
    pub at: Mutex<Point>,
    pub performed: Mutex<Vec<Step>>,
    /// When set, every `perform` fails with it — for exercising the
    /// `Suspended` and `Blocked` paths without a desktop to block on.
    pub fail: Option<InputError>,
}

impl MockPointer {
    pub fn new(screens: Screens, at: Point) -> Self {
        Self {
            screens,
            at: Mutex::new(at),
            performed: Mutex::new(Vec::new()),
            fail: None,
        }
    }

    pub fn failing(screens: Screens, at: Point, fail: InputError) -> Self {
        Self {
            fail: Some(fail),
            ..Self::new(screens, at)
        }
    }

    pub fn steps(&self) -> Vec<Step> {
        self.performed.lock().unwrap().clone()
    }

    /// Just the move targets, which is what most assertions are about.
    pub fn track(&self) -> Vec<Point> {
        self.steps()
            .into_iter()
            .filter_map(|s| match s {
                Step::Move { x, y } => Some(Point::new(x, y)),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl Pointer for MockPointer {
    async fn screens(&self) -> Result<Screens, InputError> {
        Ok(self.screens.clone())
    }

    async fn position(&self) -> Result<Point, InputError> {
        Ok(*self.at.lock().unwrap())
    }

    async fn perform(&self, steps: &[Step]) -> Result<u64, InputError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        self.performed.lock().unwrap().extend_from_slice(steps);
        if let Some(Step::Move { x, y }) =
            steps.iter().rev().find(|s| matches!(s, Step::Move { .. }))
        {
            *self.at.lock().unwrap() = Point::new(*x, *y);
        }
        Ok(self.screens.state)
    }
}
