#![cfg(any(feature = "actix2", feature = "actix3"))]

#[cfg(feature = "actix2")]
extern crate actix_web2 as actix_web;
#[cfg(feature = "actix3")]
extern crate actix_web3 as actix_web;

#[cfg(feature = "swagger-ui")]
use include_dir::{include_dir, Dir};

pub mod web;

pub use self::web::{Resource, Route, Scope};
pub use paperclip_macros::{
    api_v2_errors, api_v2_errors_overlay, api_v2_operation, delete, get, post, put, Apiv2Schema,
    Apiv2Security,
};

use self::web::{RouteWrapper, ServiceConfig};
use actix_service::ServiceFactory;
#[cfg(feature = "swagger-ui")]
use actix_web::web::HttpRequest;
use actix_web::{
    dev::{HttpServiceFactory, MessageBody, ServiceRequest, ServiceResponse, Transform},
    web::HttpResponse,
    Error,
};
use futures::future::{ok as fut_ok, Ready};
use paperclip_core::v2::models::{
    DefaultApiRaw, DefaultOperationRaw, DefaultPathItemRaw, DefaultSchemaRaw, HttpMethod,
    SecurityScheme,
};
use parking_lot::RwLock;

use std::{collections::BTreeMap, fmt::Debug, future::Future, sync::Arc};

/// Wrapper for [`actix_web::App`](https://docs.rs/actix-web/*/actix_web/struct.App.html).
pub struct App<T, B> {
    spec: Arc<RwLock<DefaultApiRaw>>,
    #[cfg(feature = "v3")]
    spec_v3: Option<Arc<RwLock<openapiv3::OpenAPI>>>,
    #[cfg(feature = "swagger-ui")]
    spec_path: Option<String>,
    inner: Option<actix_web::App<T, B>>,
}

#[cfg(feature = "swagger-ui")]
static SWAGGER_DIST: Dir = include_dir!("./swagger-ui/dist");

/// Extension trait for actix-web applications.
pub trait OpenApiExt<T, B> {
    type Wrapper;

    /// Consumes this app and produces its wrapper to start tracking
    /// paths and their corresponding operations.
    fn wrap_api(self) -> Self::Wrapper;

    /// Same as `wrap_api` initializing with provided specification
    /// defaults. Useful for defining Api properties outside of definitions and
    /// paths.
    fn wrap_api_with_spec(self, spec: DefaultApiRaw) -> Self::Wrapper;
}

impl<T, B> OpenApiExt<T, B> for actix_web::App<T, B> {
    type Wrapper = App<T, B>;

    fn wrap_api(self) -> Self::Wrapper {
        App {
            spec: Arc::new(RwLock::new(DefaultApiRaw::default())),
            #[cfg(feature = "v3")]
            spec_v3: None,
            #[cfg(feature = "swagger-ui")]
            spec_path: None,
            inner: Some(self),
        }
    }

    fn wrap_api_with_spec(self, spec: DefaultApiRaw) -> Self::Wrapper {
        App {
            spec: Arc::new(RwLock::new(spec)),
            #[cfg(feature = "v3")]
            spec_v3: None,
            #[cfg(feature = "swagger-ui")]
            spec_path: None,
            inner: Some(self),
        }
    }
}

/// Indicates that this thingmabob has a path and a bunch of definitions and operations.
pub trait Mountable {
    /// Where this thing gets mounted.
    fn path(&self) -> &str;

    /// Map of HTTP methods and the associated API operations.
    fn operations(&mut self) -> BTreeMap<HttpMethod, DefaultOperationRaw>;

    /// The definitions recorded by this object.
    fn definitions(&mut self) -> BTreeMap<String, DefaultSchemaRaw>;

    /// The security definitions recorded by this object.
    fn security_definitions(&mut self) -> BTreeMap<String, SecurityScheme>;

    /// Updates the given map of operations with operations tracked by this object.
    ///
    /// **NOTE:** Overriding implementations must ensure that the `PathItem`
    /// is normalized before updating the input map.
    fn update_operations(&mut self, map: &mut BTreeMap<String, DefaultPathItemRaw>) {
        let op_map = map
            .entry(self.path().into())
            .or_insert_with(Default::default);
        op_map.methods.extend(self.operations().into_iter());
    }
}

impl<T, B> App<T, B>
where
    B: MessageBody,
    T: ServiceFactory<
        Config = (),
        Request = ServiceRequest,
        Response = ServiceResponse<B>,
        Error = Error,
        InitError = (),
    >,
{
    /// Proxy for [`actix_web::App::data`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.data).
    ///
    /// **NOTE:** This doesn't affect spec generation.
    pub fn data<U: 'static>(mut self, data: U) -> Self {
        self.inner = self.inner.take().map(|a| a.data(data));
        self
    }

    /// Proxy for [`actix_web::App::data_factory`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.data_factory).
    ///
    /// **NOTE:** This doesn't affect spec generation.
    pub fn data_factory<F, Out, D, E>(mut self, data: F) -> Self
    where
        F: Fn() -> Out + 'static,
        Out: Future<Output = Result<D, E>> + 'static,
        D: 'static,
        E: Debug,
    {
        self.inner = self.inner.take().map(|a| a.data_factory(data));
        self
    }

    /// Proxy for [`actix_web::App::app_data`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.app_data).
    ///
    /// **NOTE:** This doesn't affect spec generation.
    pub fn app_data<U: 'static>(mut self, data: U) -> Self {
        self.inner = self.inner.take().map(|a| a.app_data(data));
        self
    }

    /// Wrapper for [`actix_web::App::configure`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.configure).
    pub fn configure<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut ServiceConfig),
    {
        self.inner = self.inner.take().map(|s| {
            s.configure(|c| {
                let mut cfg = ServiceConfig::from(c);
                f(&mut cfg);
                self.update_from_mountable(&mut cfg);
            })
        });
        self
    }

    /// Wrapper for [`actix_web::App::route`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.route).
    pub fn route(mut self, path: &str, route: Route) -> Self {
        let mut w = RouteWrapper::from(path, route);
        self.update_from_mountable(&mut w);
        self.inner = self.inner.take().map(|a| a.route(path, w.inner));
        self
    }

    /// Wrapper for [`actix_web::App::service`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.service).
    pub fn service<F>(mut self, mut factory: F) -> Self
    where
        F: Mountable + HttpServiceFactory + 'static,
    {
        self.update_from_mountable(&mut factory);
        self.inner = self.inner.take().map(|a| a.service(factory));
        self
    }

    /// Proxy for [`actix_web::App::default_service`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.default_service).
    ///
    /// **NOTE:** This doesn't affect spec generation.
    pub fn default_service<F, U>(mut self, f: F) -> Self
    where
        F: actix_service::IntoServiceFactory<U>,
        U: ServiceFactory<
                Config = (),
                Request = ServiceRequest,
                Response = ServiceResponse,
                Error = Error,
                InitError = (),
            > + 'static,
        U::InitError: Debug,
    {
        self.inner = self.inner.take().map(|a| a.default_service(f));
        self
    }

    /// Proxy for [`actix_web::App::external_resource`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.external_resource).
    ///
    /// **NOTE:** This doesn't affect spec generation.
    pub fn external_resource<N, U>(mut self, name: N, url: U) -> Self
    where
        N: AsRef<str>,
        U: AsRef<str>,
    {
        self.inner = self.inner.take().map(|a| a.external_resource(name, url));
        self
    }

    /// Proxy for [`actix_web::web::App::wrap`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.wrap).
    ///
    /// **NOTE:** This doesn't affect spec generation.
    pub fn wrap<M, B1>(
        mut self,
        mw: M,
    ) -> App<
        impl ServiceFactory<
            Config = (),
            Request = ServiceRequest,
            Response = ServiceResponse<B1>,
            Error = Error,
            InitError = (),
        >,
        B1,
    >
    where
        M: Transform<
            T::Service,
            Request = ServiceRequest,
            Response = ServiceResponse<B1>,
            Error = Error,
            InitError = (),
        >,
        B1: MessageBody,
    {
        App {
            spec: self.spec,
            #[cfg(feature = "v3")]
            spec_v3: self.spec_v3,
            #[cfg(feature = "swagger-ui")]
            spec_path: None,
            inner: self.inner.take().map(|a| a.wrap(mw)),
        }
    }

    /// Proxy for [`actix_web::web::App::wrap_fn`](https://docs.rs/actix-web/*/actix_web/struct.App.html#method.wrap_fn).
    ///
    /// **NOTE:** This doesn't affect spec generation.
    pub fn wrap_fn<B1, F, R>(
        mut self,
        mw: F,
    ) -> App<
        impl ServiceFactory<
            Config = (),
            Request = ServiceRequest,
            Response = ServiceResponse<B1>,
            Error = Error,
            InitError = (),
        >,
        B1,
    >
    where
        B1: MessageBody,
        F: FnMut(ServiceRequest, &mut T::Service) -> R + Clone,
        R: Future<Output = Result<ServiceResponse<B1>, Error>>,
    {
        App {
            spec: self.spec,
            #[cfg(feature = "v3")]
            spec_v3: self.spec_v3,
            #[cfg(feature = "swagger-ui")]
            spec_path: None,
            inner: self.inner.take().map(|a| a.wrap_fn(mw)),
        }
    }

    /// Mounts the specification for all operations and definitions
    /// recorded by the wrapper and serves them in the given path
    /// as a JSON.
    pub fn with_json_spec_at(mut self, path: &str) -> Self {
        #[cfg(feature = "swagger-ui")]
        {
            self.spec_path = Some(path.to_owned());
        }

        self.inner = self.inner.take().map(|a| {
            a.service(
                actix_web::web::resource(path)
                    .route(actix_web::web::get().to(SpecHandler(self.spec.clone()))),
            )
        });
        self
    }

    #[cfg(feature = "v3")]
    /// Converts the generated v2 specification to v3 and then
    /// mounts the v3 specification for all operations and definitions
    /// recorded by the wrapper and serves them in the given path
    /// as a JSON.
    pub fn with_json_spec_v3_at(mut self, path: &str) -> Self {
        let spec_v3 = if let Some(spec_v3) = &self.spec_v3 {
            spec_v3.clone()
        } else {
            let spec_v3 = Arc::new(RwLock::new(openapiv3::OpenAPI::default()));
            self.spec_v3 = Some(spec_v3.clone());
            spec_v3
        };
        self.inner = self.inner.take().map(|a| {
            a.service(
                actix_web::web::resource(path)
                    .route(actix_web::web::get().to(SpecHandlerV3(spec_v3.clone()))),
            )
        });
        self
    }

    /// Calls the given function with `App` and JSON `Value` representing your API
    /// specification **built until now**.
    ///
    /// **NOTE:** Unlike `with_json_spec_at`, this only has the API spec built until
    /// this function call. Any route handler added after this call won't affect the
    /// spec. So, it's important to call this function after adding all route handlers.
    pub fn with_raw_json_spec<F>(self, mut call: F) -> Self
    where
        F: FnMut(Self, serde_json::Value) -> Self,
    {
        let spec = serde_json::to_value(&*self.spec.read()).expect("generating json spec");
        call(self, spec)
    }

    #[cfg(feature = "v3")]
    /// Calls the given function with `App` and JSON `Value` representing your API
    /// v2 specification **built until now** which is converted to v3.
    ///
    /// **NOTE:** Unlike `with_json_spec_at`, this only has the API spec built until
    /// this function call. Any route handler added after this call won't affect the
    /// spec. So, it's important to call this function after adding all route handlers.
    pub fn with_raw_json_spec_v3<F>(self, mut call: F) -> Self
    where
        F: FnMut(Self, serde_json::Value) -> Self,
    {
        let v3 = paperclip_core::v3::openapiv2_to_v3(self.spec.read().clone());
        let spec = serde_json::to_value(v3).expect("generating json spec");
        call(self, spec)
    }

    /// Exposes the previously built JSON specification with Swagger UI at the given path
    ///
    /// **NOTE:** you **MUST** call with_json_spec_at before calling this function
    #[cfg(feature = "swagger-ui")]
    pub fn with_swagger_ui_at(mut self, path: &str) -> Self {
        let spec_path = self.spec_path.clone().expect(
            "Specification not set, be sure to call `with_json_spec_at` before this function",
        );

        let path: String = path.into();
        // Grab any file request from the documentation UI path and fetch it from SWAGGER_DIST
        // E.g: js, html, svg and etc.
        let regex_path = format!("{}/{{filename:.*}}", path);

        self.inner = self.inner.take().map(|a| {
            a.service(
                actix_web::web::resource([regex_path.to_owned(), path.clone()]).route(
                    actix_web::web::get().to(move |request: HttpRequest| {
                        let filename = request.match_info().query("filename");
                        if filename.is_empty() && request.query_string().is_empty() {
                            let redirect_url = format!("{}/index.html?url={}", path, spec_path);
                            HttpResponse::PermanentRedirect()
                                .header("Location", redirect_url)
                                .finish()
                        } else {
                            HttpResponse::Ok().body(
                                SWAGGER_DIST
                                    .get_file(filename)
                                    .unwrap()
                                    .contents_utf8()
                                    .unwrap(),
                            )
                        }
                    }),
                ),
            )
        });
        self
    }

    /// Builds and returns the `actix_web::App`.
    pub fn build(self) -> actix_web::App<T, B> {
        #[cfg(feature = "v3")]
        self.spec_v3.clone().map(|v3| {
            let mut v3 = v3.write();
            *v3 = paperclip_core::v3::openapiv2_to_v3(self.spec.read().clone());
        });
        self.inner.expect("missing app?")
    }

    /// Trim's the Api base path from the start of all method paths.
    /// **NOTE:** much like `with_raw_json_spec` this only has the API spec built until
    /// this function call. Any route handler added after this call won't have the base path trimmed.
    /// So, it's important to call this function after adding all route handlers.
    pub fn trim_base_path(self) -> Self {
        {
            let mut spec = self.spec.write();
            let base_path = spec.base_path.clone().unwrap_or_default();
            spec.paths = spec.paths.iter().fold(BTreeMap::new(), |mut i, (k, v)| {
                i.insert(
                    k.trim_start_matches(base_path.as_str()).to_string(),
                    v.clone(),
                );
                i
            });
        }
        self
    }

    /// Updates the underlying spec with definitions and operations from the given factory.
    fn update_from_mountable<F>(&mut self, factory: &mut F)
    where
        F: Mountable,
    {
        let mut api = self.spec.write();
        api.definitions.extend(factory.definitions().into_iter());
        SecurityScheme::append_map(
            factory.security_definitions(),
            &mut api.security_definitions,
        );
        factory.update_operations(&mut api.paths);
        if cfg!(feature = "normalize") {
            for map in api.paths.values_mut() {
                map.normalize();
            }
        }
    }
}

#[derive(Clone)]
struct SpecHandler(Arc<RwLock<DefaultApiRaw>>);

impl actix_web::dev::Factory<(), Ready<Result<HttpResponse, Error>>, Result<HttpResponse, Error>>
    for SpecHandler
{
    fn call(&self, _: ()) -> Ready<Result<HttpResponse, Error>> {
        fut_ok(HttpResponse::Ok().json(&*self.0.read()))
    }
}

#[cfg(feature = "v3")]
#[derive(Clone)]
struct SpecHandlerV3(Arc<RwLock<openapiv3::OpenAPI>>);

#[cfg(feature = "v3")]
impl actix_web::dev::Factory<(), Ready<Result<HttpResponse, Error>>, Result<HttpResponse, Error>>
    for SpecHandlerV3
{
    fn call(&self, _: ()) -> Ready<Result<HttpResponse, Error>> {
        fut_ok(HttpResponse::Ok().json(&*self.0.read()))
    }
}
